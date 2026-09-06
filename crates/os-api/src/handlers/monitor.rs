//! `MonitorRouteHandler` —— 系统监控桌面应用的 HTTP→真实系统指标适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/monitor/*`）翻译为**真实**系统指标读取 +
//! SQLite 持久化告警 + 阈值规则引擎，返回 JSON。这是 OS 系统监控桌面应用的后端
//! REST 入口。
//!
//! # 当前实现策略
//!
//! - **系统指标**（`/metrics`）：`spawn_blocking` 真实读 `/proc/loadavg`、
//!   `/proc/meminfo`、`/proc/stat`（两次采样算 CPU 使用率）、`/proc/net/dev`、
//!   `/proc/uptime`、`statvfs`（磁盘）、`/proc/sys/kernel/osrelease`、数 `/proc/[pid]`。
//!   单项读取失败时该项回退保守值（0 或默认），不拉垮整次聚合（参考 `system.rs` /
//!   `discover.rs` 的尽力探测降级语义）。
//! - **服务状态**（`/services`）：探测 `os-api` / `osd` / `sshd` / `zfs` 进程是否在跑
//!   （读 `/proc` 扫描 cmdline 或 `pgrep`）。失败回退 unknown。
//! - **告警**（`/alerts`）：**SQLite 持久化**（`alerts` 表），首次建表时 seed 2 个示例
//!   告警。`/alerts` 查最近 100 条（按时间倒序），`/ack` 把 `acked` 置 1。
//! - **阈值规则引擎**（后台 `tokio` task，60 秒一轮）：每轮拉一次真实指标 + 服务状态，
//!   套用 `check_thresholds` 纯函数 + 服务停止探测，命中规则且（同 source+level 5 分钟
//!   内未重复）时 INSERT 到 `alerts` 表。
//! - **历史**（`/history`）：占位示例数据（若干时间点 CPU/内存采样）。
//! - **ZFS 池**（`/zpools`）：真实 `zpool list -H`，失败降级为示例。
//!
//! # 路由表
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/monitor/metrics`           | 系统指标（真实 /proc 读取）|
//! | GET    | `/api/v1/monitor/net-rate`          | 实时网速（两次 /proc/net/dev 差值）|
//! | GET    | `/api/v1/monitor/services`          | 服务状态（探测进程）|
//! | GET    | `/api/v1/monitor/alerts`            | 告警列表（SQLite 持久化）|
//! | POST   | `/api/v1/monitor/alerts/:id/ack`    | 确认告警（需 admin）|
//! | GET    | `/api/v1/monitor/history`           | 历史采样（占位示例）|
//! | GET    | `/api/v1/monitor/zpools`            | ZFS 池状态（真实 zpool list）|
//! | GET    | `/api/v1/monitor/stats`             | 聚合摘要 |
//!
//! # 实时网速（`/net-rate`，2026-08-23）
//!
//! `/metrics` 的 `net_rx_bytes`/`net_tx_bytes` 是**开机以来的累计字节数**（单调
//! 计数器），不是速率——监控悬浮框此前把它当 B/s 展示导致"网速不对"。`/net-rate`
//! 读 `/proc/net/dev`（排除 `lo`），handler 内存态保存上次采样
//! （接口名 → (rx_bytes, tx_bytes) + 采样时刻），每次调用算差值得各接口与总计的
//! **字节/秒**速率；首次调用（无上次采样）返回全 0 并记录基线，下一轮差值生效。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 实时系统指标（真实读取 /proc + statvfs）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemMetrics {
    pub hostname: String,
    pub uptime_secs: u64,
    /// 1/5/15 分钟负载。
    pub load_avg: [f64; 3],
    /// CPU 使用率百分比（0-100）。
    pub cpu_usage: f32,
    pub cpu_cores: u32,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
    pub mem_available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub processes: u32,
    pub kernel_version: String,
}

/// 服务运行状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    /// `running` / `stopped` / `unknown`。
    pub status: String,
    pub pid: Option<u32>,
}

/// 一条告警（SQLite 持久化；JSON 字段 `timestamp` 对应表列 `created_at`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: String,
    /// `info` / `warning` / `critical`。
    pub level: String,
    pub message: String,
    /// `cpu` / `memory` / `disk` / `service`。
    pub source: String,
    pub timestamp: String,
    pub acked: bool,
}

/// ZFS 池状态（一行一个池）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZpoolStatus {
    pub name: String,
    /// `ONLINE` / `DEGRADED` / `OFFLINE` / `UNKNOWN`。
    pub state: String,
    pub size_bytes: u64,
    pub allocated_bytes: u64,
    pub free_bytes: u64,
    /// 健康度布尔（state == ONLINE）。
    pub healthy: bool,
}

/// 单接口实时网速（两次 `/proc/net/dev` 采样的字节差 ÷ 秒；`_bps` = bytes/s）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetIfaceRate {
    /// 接口名（如 `eth0`；已排除 `lo`）。
    pub iface: String,
    /// 下行速率（字节/秒）。
    pub rx_bps: u64,
    /// 上行速率（字节/秒）。
    pub tx_bps: u64,
}

/// 聚合总速率（全部非 lo 接口之和；`_bps` = bytes/s）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetRateSummary {
    pub rx_bps: u64,
    pub tx_bps: u64,
}

/// `GET /api/v1/monitor/net-rate` 响应：总速率 + 各接口明细（按接口名排序）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetRateSnapshot {
    pub total: NetRateSummary,
    pub interfaces: Vec<NetIfaceRate>,
}

// ----------------------------------------------------------------------------
// MonitorRouteHandler
// ----------------------------------------------------------------------------

/// 系统监控路由处理器——HTTP 边界适配到真实系统指标 + SQLite 持久化告警。
///
/// 持有：
/// - `db: Arc<Mutex<Connection>>`（SQLite：alerts 表；短锁快查快放，不跨 `.await` 持锁）；
/// - `last_cpu: Mutex<Option<(Instant, CpuSample)>>`（两次 `/proc/stat` 采样算 CPU%）；
/// - `last_net: Mutex<Option<(Instant, HashMap<iface, (rx_bytes, tx_bytes)>)>>`
///   （两次 `/proc/net/dev` 采样算实时网速——`/net-rate` 端点内存态）；
/// - `counter: Mutex<u64>`（生成告警 id 的自增计数）。
///
/// `db` 用 `Arc` 是为了把一个 clone 交给后台阈值规则引擎 task（`spawn_alert_engine`），
/// 该 task 与本 handler 共享同一 SQLite 文件。
pub struct MonitorRouteHandler {
    db: Arc<Mutex<Connection>>,
    last_cpu: Mutex<Option<(std::time::Instant, CpuSample)>>,
    last_net: Mutex<Option<NetSample>>,
    counter: Mutex<u64>,
}

/// 一次网络计数采样（`/proc/net/dev` 快照：采样时刻 + 各接口 {rx_bytes, tx_bytes}）。
type NetSample = (std::time::Instant, HashMap<String, (u64, u64)>);

/// 一次 CPU 时间采样（/proc/stat 的 cpu 聚合行）。
#[derive(Debug, Clone, Copy, Default)]
struct CpuSample {
    /// 总时间（user+nice+system+idle+iowait+irq+softirq+steal）。
    total: u64,
    /// 空闲时间（idle+iowait）。
    idle: u64,
}

impl MonitorRouteHandler {
    /// 构造 handler：打开/创建 SQLite 文件并建表，首次空表 seed 2 个示例告警。
    #[must_use]
    pub fn new() -> Self {
        Self::with_db_path(&default_db_path())
    }

    /// 用指定 DB 路径构造（生产/测试注入）。
    ///
    /// 打开文件 → 建表（IF NOT EXISTS）→ seed demo 告警（仅当 alerts 表为空时）。
    /// 打开失败时降级到内存库（绝不 panic，与上游降级语义一致）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        let conn = open_db(path).unwrap_or_else(|e| {
            eprintln!("monitor: 打开 SQLite {path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        let max_id = Self::compute_max_alert_id(&conn);
        Self {
            db: Arc::new(Mutex::new(conn)),
            last_cpu: Mutex::new(None),
            last_net: Mutex::new(None),
            counter: Mutex::new(max_id.max(100)),
        }
    }

    /// 用临时内存库构造（测试注入：数据隔离，进程结束即丢）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self {
            db: Arc::new(Mutex::new(conn)),
            last_cpu: Mutex::new(None),
            last_net: Mutex::new(None),
            counter: Mutex::new(100),
        }
    }

    /// 用临时内存库构造并 seed 2 个 demo 告警（测试注入：每个实例独立隔离，
    /// 避免 `new()` 的共享文件库在并行测试下互相干扰）。
    #[must_use]
    pub fn with_demo_data() -> Self {
        let h = Self::with_empty();
        {
            let conn = h.db.lock().expect("db poisoned");
            seed_demo_alerts(&conn).expect("seed demo 告警必成功");
        }
        h
    }

    /// 扫描 alerts 表 id 数字后缀取最大值（初始化 counter，避免重启后 id 碰撞）。
    fn compute_max_alert_id(conn: &Connection) -> u64 {
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM alerts")
            .and_then(|mut s| {
                let rows = s.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .unwrap_or_default();
        ids.iter()
            .filter_map(|id| id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok()))
            .max()
            .unwrap_or(0)
    }

    /// 生成下一个告警 id（`alert-<n>`，自增）。
    #[allow(dead_code)]
    fn next_alert_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("alert-{}", *c)
    }

    /// 当前告警快照（按时间倒序，最近 100 条）。
    #[must_use]
    pub fn alerts_snapshot(&self) -> Vec<Alert> {
        let conn = self.db.lock().expect("db poisoned");
        list_alerts(&conn, 100)
    }

    /// 算 CPU 使用率：读两次 /proc/stat（间隔 sleep），用差值算 busy/total。
    /// 首次调用（无上次采样）返回 0.0 并记录本次采样。
    fn cpu_usage_delta(&self, current: CpuSample) -> f32 {
        let mut slot = self.last_cpu.lock().expect("cpu slot poisoned");
        if let Some((_, prev)) = *slot {
            let total_d = current.total.saturating_sub(prev.total);
            let idle_d = current.idle.saturating_sub(prev.idle);
            let usage = if total_d > 0 {
                let busy_d = total_d.saturating_sub(idle_d);
                (busy_d as f32 / total_d as f32) * 100.0
            } else {
                0.0
            };
            *slot = Some((std::time::Instant::now(), current));
            usage
        } else {
            *slot = Some((std::time::Instant::now(), current));
            0.0
        }
    }

    /// 实时网速快照（GET /api/v1/monitor/net-rate 的内核）：
    /// 读 `/proc/net/dev`（排除 lo）→ 与上次采样（handler 内存态）做差 →
    /// 各接口与总计的字节/秒速率。首次调用（无上次采样）全 0 并记录基线，
    /// 下一轮差值生效（与 `cpu_usage_delta` 同款跨请求采样语义）。
    fn net_rate_snapshot(&self) -> NetRateSnapshot {
        let content = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
        let current = parse_proc_net_dev(&content);
        let now = std::time::Instant::now();
        let mut slot = self.last_net.lock().expect("net slot poisoned");
        let snapshot = net_rate_delta(slot.as_ref(), now, &current);
        *slot = Some((now, current));
        snapshot
    }

    /// 插入一条告警（分配 id + 写 DB）。返回写入后的 Alert。
    #[allow(dead_code)]
    fn insert_alert_internal(&self, level: &str, message: &str, source: &str) -> Alert {
        let alert = Alert {
            id: self.next_alert_id(),
            level: level.to_string(),
            message: message.to_string(),
            source: source.to_string(),
            timestamp: now_iso(),
            acked: false,
        };
        let conn = self.db.lock().expect("db poisoned");
        // 插入失败不 panic（与上游降级语义一致），仅打 stderr
        if let Err(e) = insert_alert(&conn, &alert) {
            eprintln!("monitor: 插入告警失败（{e}）：{level} {source} {message}");
        }
        alert
    }

    /// 启动后台阈值规则引擎（60 秒一轮，独立 `tokio` task）。
    ///
    /// 必须在 tokio 运行时上下文里调用（生产 `main.rs` 注册 handler 前调用一次）。
    /// task 持有 `db` 的 `Arc` clone，与 handler 共享同一 SQLite 文件。
    /// 每轮：`spawn_blocking` 读真实指标 + 服务状态 → `check_thresholds` →
    /// 服务停止探测 → CPU "持续 3 轮" 过滤 →（同 source+level 5 分钟内未重复）→ INSERT。
    /// 任何环节失败仅打 stderr，下一轮继续，绝不 panic。
    pub fn spawn_alert_engine(&self) {
        let db = Arc::clone(&self.db);
        tokio::spawn(async move {
            // CPU "持续过高"需要连续 3 轮都 > 85% 才触发
            let mut cpu_high_streak = 0u32;
            // 首轮立即跑一次（便于生产环境尽快产出告警），之后 60s 一轮
            let mut first = true;
            loop {
                if !first {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
                first = false;
                // 1. 读真实指标（含 CPU 两采样 delta）+ 服务状态，spawn_blocking 跑
                let metrics = tokio::task::spawn_blocking(read_metrics_with_cpu_sync)
                    .await
                    .unwrap_or_default();
                let services = tokio::task::spawn_blocking(detect_services_sync)
                    .await
                    .unwrap_or_default();
                // 2. CPU 连续计数（>85% 累加，否则归零）
                if metrics.cpu_usage > 85.0 {
                    cpu_high_streak = cpu_high_streak.saturating_add(1);
                } else {
                    cpu_high_streak = 0;
                }
                // 3. 阈值纯函数 → 候选告警
                let mut candidates = check_thresholds(&metrics);
                // 4. CPU 告警仅在连续 3 轮后保留（未达 3 轮则丢弃 cpu 候选）
                if cpu_high_streak < 3 {
                    candidates.retain(|a| a.source != "cpu");
                }
                // 5. 服务停止 → critical（os-api / sshd 停了）
                for s in &services {
                    if s.status == "stopped" && (s.name == "os-api" || s.name == "sshd") {
                        candidates.push(Alert {
                            id: String::new(),
                            level: "critical".into(),
                            message: format!("关键服务 {} 已停止", s.name),
                            source: "service".into(),
                            timestamp: String::new(),
                            acked: false,
                        });
                    }
                }
                // 6. 去重 + 写库（同 source+level 5 分钟内不重复）
                let conn_guard = match db.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        eprintln!("monitor: 引擎获取 db 锁失败（{e}），跳过本轮");
                        continue;
                    }
                };
                for a in &candidates {
                    if recent_alert_exists(&conn_guard, &a.source, &a.level, 300) {
                        continue;
                    }
                    // 分配 id + 时间戳再写
                    let to_write = Alert {
                        id: next_engine_alert_id(&conn_guard),
                        level: a.level.clone(),
                        message: a.message.clone(),
                        source: a.source.clone(),
                        timestamp: now_iso(),
                        acked: false,
                    };
                    if let Err(e) = insert_alert(&conn_guard, &to_write) {
                        eprintln!("monitor: 引擎写告警失败（{e}）：{}", a.source);
                    }
                }
            }
        });
    }
}

/// 引擎侧生成告警 id（扫表取 max+1，避免与 handler counter 不一致）。
fn next_engine_alert_id(conn: &Connection) -> String {
    let max = conn
        .query_row(
            "SELECT MAX(CAST(
                CASE WHEN substr(id,1,6)='alert-' AND substr(id,7) GLOB '[0-9]*'
                     THEN substr(id,7) ELSE '0' END AS INTEGER)
             FROM alerts",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u64;
    format!("alert-{}", max.saturating_add(1))
}

impl Default for MonitorRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for MonitorRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/monitor/metrics", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/monitor/net-rate", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/monitor/services", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/monitor/alerts", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/monitor/alerts/:id/ack",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/monitor/history", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/monitor/zpools", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/monitor/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/monitor/metrics —— 真实系统指标
            (HttpMethod::Get, ["api", "v1", "monitor", "metrics"]) => {
                let metrics = read_metrics_blocking(self).await;
                Ok(ok_json(to_value(&metrics)?))
            }

            // —— GET /api/v1/monitor/net-rate —— 实时网速（差值采样，公开）
            //    → {total: {rx_bps, tx_bps}, interfaces: [{iface, rx_bps, tx_bps}]}
            //    （bps = 字节/秒；首次调用全 0 记基线，下一轮生效）
            (HttpMethod::Get, ["api", "v1", "monitor", "net-rate"]) => {
                let snapshot = self.net_rate_snapshot();
                Ok(ok_json(to_value(&snapshot)?))
            }

            // —— GET /api/v1/monitor/services —— 服务状态
            (HttpMethod::Get, ["api", "v1", "monitor", "services"]) => {
                let services = detect_services_blocking().await;
                Ok(ok_json(to_value(&services)?))
            }

            // —— GET /api/v1/monitor/alerts —— 告警列表
            (HttpMethod::Get, ["api", "v1", "monitor", "alerts"]) => {
                let alerts = self.alerts_snapshot();
                Ok(ok_json(to_value(&alerts)?))
            }

            // —— POST /api/v1/monitor/alerts/:id/ack —— 确认告警（SQLite UPDATE）
            (HttpMethod::Post, ["api", "v1", "monitor", "alerts", id, "ack"]) => {
                let conn = self.db.lock().expect("db poisoned");
                match ack_alert(&conn, id) {
                    Ok(Some(a)) => Ok(ok_json(to_value(&a)?)),
                    Ok(None) => Ok(error_response(404, &format!("告警不存在: {id}"))),
                    Err(e) => Ok(error_response(500, &format!("确认告警失败: {e}"))),
                }
            }

            // —— GET /api/v1/monitor/history —— 占位历史采样
            (HttpMethod::Get, ["api", "v1", "monitor", "history"]) => Ok(ok_json(demo_history())),

            // —— GET /api/v1/monitor/zpools —— ZFS 池状态（真实 zpool list，失败降级）
            (HttpMethod::Get, ["api", "v1", "monitor", "zpools"]) => {
                let pools = list_zpools_blocking().await;
                Ok(ok_json(to_value(&pools)?))
            }

            // —— GET /api/v1/monitor/stats —— 聚合摘要
            (HttpMethod::Get, ["api", "v1", "monitor", "stats"]) => {
                let metrics = read_metrics_blocking(self).await;
                let alerts = self.alerts_snapshot();
                let unacked = alerts.iter().filter(|a| !a.acked).count();
                let pools = list_zpools_blocking().await;
                let healthy_pools = pools.iter().filter(|p| p.healthy).count();
                Ok(ok_json(serde_json::json!({
                    "cpu_usage": metrics.cpu_usage,
                    "cpu_cores": metrics.cpu_cores,
                    "mem_used_ratio": mem_ratio(&metrics),
                    "disk_used_ratio": disk_ratio(&metrics),
                    "load_avg_1": metrics.load_avg[0],
                    "uptime_secs": metrics.uptime_secs,
                    "processes": metrics.processes,
                    "alerts_total": alerts.len(),
                    "alerts_unacked": unacked,
                    "zpools_total": pools.len(),
                    "zpools_healthy": healthy_pools,
                    "hostname": metrics.hostname,
                })))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "monitor: 未匹配的路由")),
        }
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
        handler_component: "monitor".to_string(),
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

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（alerts 表）
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 /tank/os-data/monitor.db，再 /var/lib/os/monitor.db，
/// 最后 ./monitor.db（保底）。
fn default_db_path() -> String {
    for p in &["/tank/os-data/monitor.db", "/var/lib/os/monitor.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./monitor.db".to_string()
}

/// 打开 SQLite 文件，建表，首次空表时 seed demo 告警。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    seed_demo_alerts_if_empty(&conn)?;
    Ok(conn)
}

/// 建 alerts 表（IF NOT EXISTS）+ created_at 索引。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS alerts (
            id TEXT PRIMARY KEY,
            level TEXT NOT NULL,
            message TEXT NOT NULL,
            source TEXT NOT NULL,
            acked INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_alerts_created_at ON alerts(created_at);
        CREATE INDEX IF NOT EXISTS idx_alerts_source_level ON alerts(source, level);
        ",
    )
}

/// 首次空表时 seed 2 个 demo 告警（CPU + 磁盘）。
fn seed_demo_alerts_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM alerts", [], |row| row.get(0))?;
    if count == 0 {
        seed_demo_alerts(conn)?;
    }
    Ok(())
}

/// 无条件 seed 2 个 demo 告警（测试 / 首次建表复用）。
fn seed_demo_alerts(conn: &Connection) -> rusqlite::Result<()> {
    for a in demo_alerts() {
        insert_alert(conn, &a)?;
    }
    Ok(())
}

/// 插入一条告警。
fn insert_alert(conn: &Connection, a: &Alert) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO alerts (id, level, message, source, acked, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            a.id,
            a.level,
            a.message,
            a.source,
            a.acked as i64,
            a.timestamp
        ],
    )?;
    Ok(())
}

/// 确认告警（acked=1）。返回更新后的 Alert，不存在返回 None。
fn ack_alert(conn: &Connection, id: &str) -> rusqlite::Result<Option<Alert>> {
    let updated = conn.execute("UPDATE alerts SET acked = 1 WHERE id = ?1", params![id])?;
    if updated == 0 {
        return Ok(None);
    }
    find_alert(conn, id)
}

/// 按 id 查单条告警。
fn find_alert(conn: &Connection, id: &str) -> rusqlite::Result<Option<Alert>> {
    let mut stmt = conn.prepare(
        "SELECT id, level, message, source, acked, created_at
         FROM alerts WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], alert_from_row)?;
    match rows.next() {
        Some(Ok(a)) => Ok(Some(a)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

/// 列最近 N 条告警（按 created_at 倒序）。
fn list_alerts(conn: &Connection, limit: usize) -> Vec<Alert> {
    let mut stmt = match conn.prepare(
        "SELECT id, level, message, source, acked, created_at
         FROM alerts ORDER BY created_at DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("monitor: 查询告警失败（{e}）");
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(params![limit as i64], alert_from_row) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("monitor: 查询告警映射失败（{e}）");
            return Vec::new();
        }
    };
    rows.filter_map(Result::ok).collect()
}

/// 是否存在同 source+level 且最近 `within_secs` 秒内的告警（去重）。
///
/// 用 SQLite `strftime('%s', ...)` 把 `created_at`（ISO8601，含时区）和 `now`
/// 都转成 epoch 秒后做整数比较，规避不同时区/格式下字符串比较的脆弱性。
fn recent_alert_exists(conn: &Connection, source: &str, level: &str, within_secs: u64) -> bool {
    let sql = "SELECT COUNT(*) FROM alerts
               WHERE source = ?1 AND level = ?2
                 AND CAST(strftime('%s', created_at) AS INTEGER)
                     >= CAST(strftime('%s', 'now') AS INTEGER) - ?3";
    conn.query_row(sql, params![source, level, within_secs as i64], |row| {
        row.get::<_, i64>(0)
    })
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 行 → Alert（acked 列 INTEGER → bool；created_at 列 → timestamp 字段）。
fn alert_from_row(row: &rusqlite::Row) -> rusqlite::Result<Alert> {
    let acked: i64 = row.get(4)?;
    Ok(Alert {
        id: row.get(0)?,
        level: row.get(1)?,
        message: row.get(2)?,
        source: row.get(3)?,
        acked: acked != 0,
        timestamp: row.get(5)?,
    })
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn mem_ratio(m: &SystemMetrics) -> f32 {
    if m.mem_total_bytes == 0 {
        0.0
    } else {
        (m.mem_used_bytes as f32 / m.mem_total_bytes as f32).clamp(0.0, 1.0)
    }
}

fn disk_ratio(m: &SystemMetrics) -> f32 {
    if m.disk_total_bytes == 0 {
        0.0
    } else {
        (m.disk_used_bytes as f32 / m.disk_total_bytes as f32).clamp(0.0, 1.0)
    }
}

// ----------------------------------------------------------------------------
// 真实系统指标读取（spawn_blocking 池跑，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 读 `/proc/stat` CPU 聚合行，返回 (total, idle)。
fn read_cpu_sample() -> Option<CpuSample> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    let first = content.lines().find(|l| l.starts_with("cpu "))?;
    let mut parts = first.split_whitespace();
    parts.next()?; // 跳过 "cpu"
                   // user nice system idle iowait irq softirq steal guest guest_nice
    let fields: Vec<u64> = parts.filter_map(|s| s.parse::<u64>().ok()).collect();
    if fields.len() < 4 {
        return None;
    }
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).unwrap_or(&0);
    let total: u64 = fields.iter().take(8).sum();
    Some(CpuSample { total, idle })
}

/// 数 CPU 核心数（`/proc/cpuinfo` 的 `processor` 行计数，失败回退 1）。
fn count_cpu_cores() -> u32 {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|c| c.lines().filter(|l| l.starts_with("processor")).count() as u32)
        .unwrap_or(1)
        .max(1)
}

/// 读 `/proc/loadavg` 返回 [1, 5, 15] 分钟负载（失败回退 [0,0,0]）。
fn read_loadavg() -> [f64; 3] {
    let content = match std::fs::read_to_string("/proc/loadavg") {
        Ok(c) => c,
        Err(_) => return [0.0, 0.0, 0.0],
    };
    let mut parts = content.split_whitespace();
    let a = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let b = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let c = parts
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    [a, b, c]
}

/// 读 `/proc/meminfo`，返回 (mem_total, mem_available, swap_total, swap_free) 字节。
///
/// `pub(crate)` 复用方（mem 使用率口径一致：used = total - available）：
/// - terminal.rs 的 node-snapshot 聚合；
/// - llm.rs / api_market.rs / media_gen.rs 的**统一内存回退**（2026-09-03，
///   DGX Spark GB10：CPU/GPU 共享 LPDDR5x，nvidia-smi 显存报 `[N/A]`，
///   真值即本池——总量/可用/已用全从这一口径出）。
pub(crate) fn read_meminfo() -> (u64, u64, u64, u64) {
    let content = match std::fs::read_to_string("/proc/meminfo") {
        Ok(c) => c,
        Err(_) => return (0, 0, 0, 0),
    };
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in content.lines() {
        let kb_to_bytes = |v: u64| v.saturating_mul(1024);
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            mem_total = kb_to_bytes(parse_first_kb(rest));
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            mem_avail = kb_to_bytes(parse_first_kb(rest));
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total = kb_to_bytes(parse_first_kb(rest));
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free = kb_to_bytes(parse_first_kb(rest));
        }
    }
    (mem_total, mem_avail, swap_total, swap_free)
}

/// 解析 meminfo 值字段首个整数（kB）。
fn parse_first_kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
}

/// 解析 `/proc/net/dev` 文本 → 接口名 → (rx_bytes, tx_bytes)（**排除 lo**）。
///
/// 行形如 `  eth0: 1234 0 0 0 0 0 0 0  5678 0 0 0 0 0 0 0`：第 1 列 rx_bytes、
/// 第 9 列 tx_bytes（内核固定 16 列统计）。表头两行（`Inter-|…` / `face |…`）
/// 无冒号分隔或字段不足，跳过；本机 `/proc/net/dev` 的实际行冒号后恰 16 个
/// 数值字段，`stats.len() > 8` 已覆盖 tx 位置。解析失败的行静默忽略。
fn parse_proc_net_dev(content: &str) -> HashMap<String, (u64, u64)> {
    let mut out = HashMap::new();
    for line in content.lines().skip(2) {
        let colon = match line.find(':') {
            Some(i) => i,
            None => continue,
        };
        let iface = line[..colon].trim();
        if iface.is_empty() || iface == "lo" {
            continue;
        }
        let stats: Vec<u64> = line[colon + 1..]
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        if let (Some(&rx), Some(&tx)) = (stats.first(), stats.get(8)) {
            out.insert(iface.to_string(), (rx, tx));
        }
    }
    out
}

/// 两次采样差值 → 实时网速（纯函数，单测直接断言）。
///
/// - `prev = None`（首次调用）：全 0 返回（基线由调用方记录——`current` 不可
///   从本函数回传，故调用方随后无条件写入 `(now, current)`）；
/// - 有上次采样：逐接口 `saturating_sub` 差值 ÷ 间隔秒数 → 字节/秒。接口计数
///   器重置/回绕 → 差值为负 → saturating 到 0；**新出现接口**（无上次采样）→
///   本轮 0（以本次读数为基线——若按 0 起算会把开机累计量错当本轮流量）；
///   上次有而本次消失的接口不再计入；间隔 ≤ 0（同刻采样）→ 全 0 防除零；
/// - `total` 为明细求和；`interfaces` 按接口名排序保证输出稳定。
fn net_rate_delta(
    prev: Option<&NetSample>,
    now: std::time::Instant,
    current: &HashMap<String, (u64, u64)>,
) -> NetRateSnapshot {
    let Some((prev_at, prev_map)) = prev else {
        return NetRateSnapshot::default();
    };
    let secs = now.saturating_duration_since(*prev_at).as_secs_f64();
    if secs <= 0.0 {
        return NetRateSnapshot::default();
    }
    let mut interfaces: Vec<NetIfaceRate> = current
        .iter()
        .map(|(iface, (rx, tx))| {
            // 新接口无上次采样 → 以本次读数为基线（差值 0），不当本轮流量
            let (prev_rx, prev_tx) = prev_map.get(iface).copied().unwrap_or((*rx, *tx));
            NetIfaceRate {
                iface: iface.clone(),
                rx_bps: ((rx.saturating_sub(prev_rx)) as f64 / secs).round() as u64,
                tx_bps: ((tx.saturating_sub(prev_tx)) as f64 / secs).round() as u64,
            }
        })
        .collect();
    interfaces.sort_by(|a, b| a.iface.cmp(&b.iface));
    let total = NetRateSummary {
        rx_bps: interfaces.iter().map(|i| i.rx_bps).sum(),
        tx_bps: interfaces.iter().map(|i| i.tx_bps).sum(),
    };
    NetRateSnapshot { total, interfaces }
}

/// 读 `/proc/net/dev` 累加各非 lo 接口的 rx/tx bytes。
fn read_net_bytes() -> (u64, u64) {
    let content = match std::fs::read_to_string("/proc/net/dev") {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in content.lines().skip(2) {
        // 行形如 "  eth0: 1234  ...  5678  ..."
        let colon = match line.find(':') {
            Some(i) => i,
            None => continue,
        };
        let iface = line[..colon].trim();
        if iface == "lo" {
            continue;
        }
        let stats: Vec<u64> = line[colon + 1..]
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .collect();
        // 字段顺序：rx_bytes rx_packets ... tx_bytes tx_packets ...
        if let Some(&r) = stats.first() {
            rx += r;
        }
        if stats.len() > 8 {
            tx += stats[8];
        }
    }
    (rx, tx)
}

/// 读 `/proc/uptime` 第一字段（秒）。
///
/// `pub(crate)`：terminal.rs 的 node-snapshot 聚合复用（同一 /proc 事实源，
/// 不重复实现解析）。
pub(crate) fn read_uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|c| c.split_whitespace().next().map(String::from))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// 数 `/proc/[0-9]+` 目录得进程数。
fn count_processes() -> u32 {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|s| s.chars().all(|c| c.is_ascii_digit()))
                        .unwrap_or(false)
                })
                .count() as u32
        })
        .unwrap_or(0)
}

/// 读 `/proc/sys/kernel/osrelease` 得内核版本（失败回退 "unknown"）。
fn read_kernel_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 探测本机主机名（`hostname` 命令；失败回退 `"local"`）。
fn detect_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// 用 `df -B1 /` 查根分区磁盘容量（总/已用字节）。
///
/// `df -B1` 以字节为单位输出（GNU coreutils）。失败 / 不可解析时返回 (0, 0)。
/// 第 1 列 filesystem、第 2 列 1K-blocks（此处 -B1 → bytes）、第 3 列 used、
/// 第 4 列 available、第 5 列 use%、第 6 列 mounted on。
///
/// `pub(crate)`：terminal.rs 的 node-snapshot 聚合复用（子进程调用，调用方
/// 需在 spawn_blocking 池里跑）。
pub(crate) fn read_root_disk() -> (u64, u64) {
    let output = std::process::Command::new("df").args(["-B1", "/"]).output();
    let out = match output {
        Ok(o) if o.status.success() => o,
        _ => return (0, 0),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // 跳过表头行，取第一数据行
    let data_line = match text.lines().nth(1) {
        Some(l) => l,
        None => return (0, 0),
    };
    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() < 4 {
        return (0, 0);
    }
    let total = parts[1].parse::<u64>().unwrap_or(0);
    let used = parts[2].parse::<u64>().unwrap_or(0);
    (total, used)
}

/// 读全部系统指标。CPU 采样在 spawn_blocking 池里跑两次（含 100ms sleep），
/// 算出的当前采样回主 async 任务，由 handler.cpu_usage_delta() 与上次跨请求采样做差。
async fn read_metrics_blocking(handler: &MonitorRouteHandler) -> SystemMetrics {
    // spawn_blocking 读全部真实指标（含两次 /proc/stat 采样 + 100ms sleep）
    let payload = tokio::task::spawn_blocking(read_metrics_payload_sync)
        .await
        .unwrap_or_else(|_| MetricsPayload::default());
    // CPU delta 由 handler 持有的跨请求上次采样算（主任务上跑，不阻塞）
    let cpu_usage = handler.cpu_usage_delta(payload.cpu_sample);
    payload.into_metrics(cpu_usage)
}

/// 引擎专用：同步读全部真实指标 + 自带 CPU 两采样 delta（不依赖 handler 跨请求状态）。
///
/// 与 `read_metrics_payload_sync` 的区别：CPU% 用本函数内部两次 `/proc/stat` 采样算
/// （间隔 100ms），便于后台引擎独立运行不共享 handler 的 `last_cpu`。失败降级不 panic。
fn read_metrics_with_cpu_sync() -> SystemMetrics {
    let s1 = read_cpu_sample();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let s2 = read_cpu_sample();
    let cpu_usage = match (s1, s2) {
        (Some(a), Some(b)) => {
            let total_d = b.total.saturating_sub(a.total);
            let idle_d = b.idle.saturating_sub(a.idle);
            if total_d > 0 {
                let busy_d = total_d.saturating_sub(idle_d);
                (busy_d as f32 / total_d as f32) * 100.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    };
    let payload = read_metrics_payload_sync();
    payload.into_metrics(cpu_usage)
}

/// 阈值规则纯函数：给定一次指标快照，返回触发的候选告警（不含 id/timestamp）。
///
/// 规则（单次快照）：
/// - CPU > 85% → critical，source=cpu，"CPU 持续过高"
/// - 内存 > 90% → warning，source=memory，"内存不足"
/// - 磁盘 > 90% → critical，source=disk，"磁盘空间不足"
///
/// 注意：CPU 的"持续 3 轮"语义由引擎循环的 `cpu_high_streak` 计数器在调用方过滤，
/// 本函数只判定单次是否过阈值。服务停止的 source=service 告警也由引擎补充
/// （`SystemMetrics` 不含服务状态）。
#[must_use]
pub fn check_thresholds(metrics: &SystemMetrics) -> Vec<Alert> {
    let mut out = Vec::new();
    if metrics.cpu_usage > 85.0 {
        out.push(Alert {
            id: String::new(),
            level: "critical".into(),
            message: format!("CPU 持续过高（{:.1}%）", metrics.cpu_usage),
            source: "cpu".into(),
            timestamp: String::new(),
            acked: false,
        });
    }
    let mem_ratio = mem_ratio(metrics);
    if mem_ratio > 0.90 {
        out.push(Alert {
            id: String::new(),
            level: "warning".into(),
            message: format!("内存不足（使用率 {:.0}%）", mem_ratio * 100.0),
            source: "memory".into(),
            timestamp: String::new(),
            acked: false,
        });
    }
    let disk_ratio = disk_ratio(metrics);
    if disk_ratio > 0.90 {
        out.push(Alert {
            id: String::new(),
            level: "critical".into(),
            message: format!("磁盘空间不足（使用率 {:.0}%）", disk_ratio * 100.0),
            source: "disk".into(),
            timestamp: String::new(),
            acked: false,
        });
    }
    out
}

/// spawn_blocking 读取的全部真实指标（CPU% 由主任务用 handler delta 算后补）。
#[derive(Debug, Default)]
struct MetricsPayload {
    cpu_sample: CpuSample,
    cpu_cores: u32,
    hostname: String,
    uptime_secs: u64,
    load_avg: [f64; 3],
    mem_total_bytes: u64,
    mem_available_bytes: u64,
    swap_total_bytes: u64,
    swap_used_bytes: u64,
    disk_total_bytes: u64,
    disk_used_bytes: u64,
    net_rx_bytes: u64,
    net_tx_bytes: u64,
    processes: u32,
    kernel_version: String,
}

impl MetricsPayload {
    fn into_metrics(self, cpu_usage: f32) -> SystemMetrics {
        let mem_used = self
            .mem_total_bytes
            .saturating_sub(self.mem_available_bytes);
        SystemMetrics {
            hostname: self.hostname,
            uptime_secs: self.uptime_secs,
            load_avg: self.load_avg,
            cpu_usage,
            cpu_cores: self.cpu_cores,
            mem_total_bytes: self.mem_total_bytes,
            mem_used_bytes: mem_used,
            mem_available_bytes: self.mem_available_bytes,
            swap_total_bytes: self.swap_total_bytes,
            swap_used_bytes: self.swap_used_bytes,
            disk_total_bytes: self.disk_total_bytes,
            disk_used_bytes: self.disk_used_bytes,
            net_rx_bytes: self.net_rx_bytes,
            net_tx_bytes: self.net_tx_bytes,
            processes: self.processes,
            kernel_version: self.kernel_version,
        }
    }
}

/// 同步读全部指标（spawn_blocking 池里跑，含两次 /proc/stat 采样 + 100ms sleep）。
/// 返回 payload（不含 CPU%，CPU% 由主任务用 handler 的跨请求 delta 算）。
fn read_metrics_payload_sync() -> MetricsPayload {
    // 首次采样 + 间隔 100ms 再采样一次（取最新采样作"当前"快照，
    // 与 handler 内上次请求采样做差算 CPU%）
    let _first = read_cpu_sample();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let cpu_sample = read_cpu_sample().unwrap_or_default();

    let (mem_total, mem_avail, swap_total, swap_free) = read_meminfo();
    let swap_used = swap_total.saturating_sub(swap_free);
    let (disk_total, disk_used) = read_root_disk();
    let (net_rx, net_tx) = read_net_bytes();

    MetricsPayload {
        cpu_sample,
        cpu_cores: count_cpu_cores(),
        hostname: detect_hostname(),
        uptime_secs: read_uptime(),
        load_avg: read_loadavg(),
        mem_total_bytes: mem_total,
        mem_available_bytes: mem_avail,
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_used,
        disk_total_bytes: disk_total,
        disk_used_bytes: disk_used,
        net_rx_bytes: net_rx,
        net_tx_bytes: net_tx,
        processes: count_processes(),
        kernel_version: read_kernel_version(),
    }
}

/// 探测关键服务进程状态（读 /proc 扫 cmdline，匹配 os-api/osd/sshd/zfs）。
async fn detect_services_blocking() -> Vec<ServiceStatus> {
    tokio::task::spawn_blocking(detect_services_sync)
        .await
        .unwrap_or_default()
}

/// 同步版服务探测：扫 /proc/*/cmdline 匹配关键字。
fn detect_services_sync() -> Vec<ServiceStatus> {
    let targets = ["os-api", "osd", "sshd", "zfs"];
    let mut found: std::collections::HashMap<&str, Option<u32>> =
        targets.iter().map(|t| (*t, None)).collect();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let pid = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok());
            let Some(pid) = pid else { continue };
            let cmdline = std::fs::read_to_string(entry.path().join("cmdline")).unwrap_or_default();
            if cmdline.is_empty() {
                continue;
            }
            for t in targets {
                if found.get(t).copied().flatten().is_none() && cmdline.contains(t) {
                    found.insert(t, Some(pid));
                }
            }
        }
    }
    targets
        .iter()
        .map(|t| ServiceStatus {
            name: t.to_string(),
            status: if found.get(t).copied().flatten().is_some() {
                "running".into()
            } else {
                "stopped".into()
            },
            pid: found.get(t).copied().flatten(),
        })
        .collect()
}

/// 列 ZFS 池（真实 `zpool list -H`，失败降级为示例池）。
async fn list_zpools_blocking() -> Vec<ZpoolStatus> {
    tokio::task::spawn_blocking(list_zpools_sync)
        .await
        .unwrap_or_else(|_| demo_zpools())
}

/// 同步版列 zpool：跑 `zpool list -H`，解析输出。
fn list_zpools_sync() -> Vec<ZpoolStatus> {
    let output = std::process::Command::new("zpool")
        .args(["list", "-H"])
        .output();
    let out = match output {
        Ok(o) if o.status.success() => o,
        _ => return demo_zpools(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: Vec<ZpoolStatus> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_zpool_line)
        .collect();
    if parsed.is_empty() {
        demo_zpools()
    } else {
        parsed
    }
}

/// 解析一行 `zpool list -H`：
/// 形如 `tank   928G   612K   928G   -   -   0%   0%   1.00x   ONLINE   -`
/// 列序：NAME SIZE ALLOC FREE CKPOINT EXPANDSZ FRAG CAP DEDUP HEALTH ALTROOT。
fn parse_zpool_line(line: &str) -> Option<ZpoolStatus> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 10 {
        return None;
    }
    let name = parts[0].to_string();
    let state = parts.get(9).copied().unwrap_or("UNKNOWN").to_string();
    let size_bytes = parse_size_to_bytes(parts.get(1).copied().unwrap_or("0"));
    let alloc_bytes = parse_size_to_bytes(parts.get(2).copied().unwrap_or("0"));
    let free_bytes = parse_size_to_bytes(parts.get(3).copied().unwrap_or("0"));
    let healthy = state == "ONLINE";
    Some(ZpoolStatus {
        name,
        state,
        size_bytes,
        allocated_bytes: alloc_bytes,
        free_bytes,
        healthy,
    })
}

/// 解析大小字段（`928G` / `1.5T`）为字节（无单位按字节）。
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

/// demo 告警（让前端首次即有可见告警）。
fn demo_alerts() -> Vec<Alert> {
    vec![
        Alert {
            id: "alert-1".into(),
            level: "warning".into(),
            message: "CPU 使用率持续超过 80%（最近 5 分钟）".into(),
            source: "cpu".into(),
            timestamp: "2026-08-08T09:15:00+08:00".into(),
            acked: false,
        },
        Alert {
            id: "alert-2".into(),
            level: "critical".into(),
            message: "tank 数据池磁盘使用率达到 92%，建议清理或扩容".into(),
            source: "disk".into(),
            timestamp: "2026-08-08T09:20:00+08:00".into(),
            acked: false,
        },
    ]
}

/// demo ZFS 池（zpool 不可用时降级显示）。
fn demo_zpools() -> Vec<ZpoolStatus> {
    vec![ZpoolStatus {
        name: "tank".into(),
        state: "ONLINE".into(),
        size_bytes: 1_000_000_000_000,
        allocated_bytes: 920_000_000_000,
        free_bytes: 80_000_000_000,
        healthy: true,
    }]
}

/// demo 历史采样（若干时间点的 cpu/mem 采样）。
fn demo_history() -> serde_json::Value {
    serde_json::json!({
        "sample_interval_secs": 60,
        "points": [
            {"t": "2026-08-08T08:00:00+08:00", "cpu": 12.5, "mem_used_ratio": 0.45, "net_rx": 1_200_000},
            {"t": "2026-08-08T08:15:00+08:00", "cpu": 35.2, "mem_used_ratio": 0.52, "net_rx": 3_400_000},
            {"t": "2026-08-08T08:30:00+08:00", "cpu": 78.9, "mem_used_ratio": 0.68, "net_rx": 8_900_000},
            {"t": "2026-08-08T08:45:00+08:00", "cpu": 82.1, "mem_used_ratio": 0.71, "net_rx": 7_200_000},
            {"t": "2026-08-08T09:00:00+08:00", "cpu": 45.6, "mem_used_ratio": 0.63, "net_rx": 2_100_000},
            {"t": "2026-08-08T09:15:00+08:00", "cpu": 22.3, "mem_used_ratio": 0.55, "net_rx": 1_500_000}
        ]
    })
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

    #[tokio::test]
    async fn routes_declares_eight_endpoints() {
        let h = MonitorRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 8);
        assert!(routes.iter().all(|r| r.handler_component == "monitor"));
        // 仅 ack 写操作需 admin
        for r in &routes {
            if r.method == HttpMethod::Post {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            } else {
                assert!(!r.requires_auth);
            }
        }
    }

    #[tokio::test]
    async fn metrics_returns_real_system_data_without_panic() {
        let h = MonitorRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/monitor/metrics")).await.unwrap();
        assert_eq!(resp.status, 200);
        // hostname 非空
        let hostname = resp.body["hostname"].as_str().expect("hostname 字符串");
        assert!(!hostname.is_empty());
        // cpu_cores 至少 1
        let cores = resp.body["cpu_cores"].as_u64().expect("cpu_cores u64");
        assert!(cores >= 1);
        // cpu_usage 在 0..=100（首次可能为 0）
        let cpu = resp.body["cpu_usage"].as_f64().expect("cpu_usage 数值");
        assert!((0.0..=100.0).contains(&cpu));
        // 进程数非负
        assert!(resp.body["processes"].as_u64().unwrap_or(0) < u32::MAX as u64);
        // load_avg 是 3 元数组
        assert_eq!(resp.body["load_avg"].as_array().unwrap().len(), 3);
    }

    // —— 实时网速（/net-rate）：解析 + 差值 + 端点行为 ——

    /// 固定样本文本：表头两行 + eth0/wlan0/lo 三接口 + 一行残缺行。
    const NET_DEV_SAMPLE: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n  eth0: 1234567    987    0    0    0     0          0         0  7654321    654    0    0    0     0       0          0\n  wlan0: 100 10 0 0 0 0 0 0  200 20 0 0 0 0 0 0\n    lo: 999999 999 0 0 0 0 0 0 999999 999 0 0 0 0 0 0\n  bad0: not-a-number\n";

    #[test]
    fn parse_proc_net_dev_parses_and_skips_lo() {
        let m = parse_proc_net_dev(NET_DEV_SAMPLE);
        // lo 排除、残缺行跳过；仅保留可完整解析的非 lo 接口
        assert_eq!(m.len(), 2, "eth0 + wlan0（lo 排除、bad0 残缺跳过）: {m:?}");
        assert_eq!(m.get("eth0"), Some(&(1_234_567, 7_654_321)));
        assert_eq!(m.get("wlan0"), Some(&(100, 200)));
        assert!(!m.contains_key("lo"), "lo 必须排除");
    }

    #[test]
    fn net_rate_delta_first_call_returns_zero() {
        let now = std::time::Instant::now();
        let current = parse_proc_net_dev(NET_DEV_SAMPLE);
        let snap = net_rate_delta(None, now, &current);
        assert_eq!(snap, NetRateSnapshot::default(), "首次调用全 0（记基线）");
    }

    #[test]
    fn net_rate_delta_computes_bps_over_interval() {
        let now = std::time::Instant::now();
        let prev_at = now
            .checked_sub(std::time::Duration::from_secs(2))
            .expect("checked_sub 2s 必成功");
        // 上次：eth0/wlan0/gone0；本次：eth0（正常增长）、wlan0（rx 计数器重置
        // → 差值为负 → 0）、new0（新接口无上次值 → 0）；gone0 消失不再计入
        let prev: HashMap<String, (u64, u64)> = HashMap::from([
            ("eth0".into(), (1_000, 500)),
            ("wlan0".into(), (5_000, 5_000)),
            ("gone0".into(), (10, 10)),
        ]);
        let current: HashMap<String, (u64, u64)> = HashMap::from([
            ("eth0".into(), (3_000, 1_500)),
            ("wlan0".into(), (4_000, 6_000)),
            ("new0".into(), (100, 100)),
        ]);
        let snap = net_rate_delta(Some(&(prev_at, prev)), now, &current);
        assert_eq!(
            snap.interfaces,
            vec![
                NetIfaceRate {
                    iface: "eth0".into(),
                    rx_bps: 1_000,
                    tx_bps: 500
                },
                NetIfaceRate {
                    iface: "new0".into(),
                    rx_bps: 0,
                    tx_bps: 0
                },
                NetIfaceRate {
                    iface: "wlan0".into(),
                    rx_bps: 0,
                    tx_bps: 500
                },
            ],
            "按接口名排序；重置/新接口差值 0，消失接口剔除"
        );
        assert_eq!(
            snap.total,
            NetRateSummary {
                rx_bps: 1_000,
                tx_bps: 1_000
            }
        );
    }

    #[test]
    fn net_rate_delta_zero_interval_avoids_division_by_zero() {
        let now = std::time::Instant::now();
        let prev: HashMap<String, (u64, u64)> = HashMap::from([("eth0".into(), (100, 100))]);
        let current: HashMap<String, (u64, u64)> = HashMap::from([("eth0".into(), (900, 900))]);
        // 同一时刻采样（elapsed=0）→ 全 0，不 panic
        let snap = net_rate_delta(Some(&(now, prev)), now, &current);
        assert_eq!(snap, NetRateSnapshot::default());
    }

    #[tokio::test]
    async fn net_rate_endpoint_baseline_then_shape() {
        let h = MonitorRouteHandler::with_empty();
        // 首次调用：记基线，全 0，但结构完整（total + interfaces）
        let resp = h.handle(get_req("/api/v1/monitor/net-rate")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"]["rx_bps"], 0, "首次调用全 0（记基线）");
        assert_eq!(resp.body["total"]["tx_bps"], 0);
        assert!(
            resp.body["interfaces"].as_array().is_some(),
            "interfaces 恒为数组"
        );
        // 第二次调用：差值生效（真机上有流量则为正；CI 静默环境可能仍 0——
        // 只断言结构与数值合法性）
        let resp = h.handle(get_req("/api/v1/monitor/net-rate")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["total"]["rx_bps"].is_u64());
        assert!(resp.body["total"]["tx_bps"].is_u64());
        for iface in resp.body["interfaces"].as_array().unwrap_or(&vec![]) {
            assert!(iface["iface"].is_string(), "明细条目带接口名: {iface}");
            assert!(iface["rx_bps"].is_u64() && iface["tx_bps"].is_u64());
        }
    }

    #[tokio::test]
    async fn services_returns_status_list_without_panic() {
        let h = MonitorRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/monitor/services")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("services 为数组");
        assert!(!arr.is_empty());
        assert!(arr.iter().all(|s| s["name"].is_string()));
        assert!(arr.iter().all(|s| s["status"].is_string()));
    }

    #[tokio::test]
    async fn alerts_returns_demo_list() {
        let h = MonitorRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/monitor/alerts")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("alerts 为数组");
        assert!(arr.len() >= 2);
        assert!(arr.iter().all(|a| a["id"].is_string()));
        assert!(arr.iter().all(|a| a["level"].is_string()));
    }

    #[tokio::test]
    async fn ack_sets_acked_true() {
        let h = MonitorRouteHandler::with_demo_data();
        // 初始未确认
        let before = h.alerts_snapshot();
        let target = before.iter().find(|a| a.id == "alert-1").unwrap();
        assert!(!target.acked);
        // ack
        let resp = h
            .handle(post_req(
                "/api/v1/monitor/alerts/alert-1/ack",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "alert-1");
        assert_eq!(resp.body["acked"], true);
        // 状态持久
        let after = h.alerts_snapshot();
        let target = after.iter().find(|a| a.id == "alert-1").unwrap();
        assert!(target.acked);
    }

    #[tokio::test]
    async fn ack_missing_returns_404() {
        let h = MonitorRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/monitor/alerts/nope/ack",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn history_returns_placeholder_points() {
        let h = MonitorRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/monitor/history")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["sample_interval_secs"].as_u64().unwrap() > 0);
        let points = resp.body["points"].as_array().expect("points 数组");
        assert!(points.len() >= 3);
        assert!(points.iter().all(|p| p["t"].is_string()));
        assert!(points.iter().all(|p| p["cpu"].is_number()));
    }

    #[tokio::test]
    async fn zpools_returns_array_without_panic() {
        // zpool 可能不可用，应降级为 demo（数组），不 panic
        let h = MonitorRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/monitor/zpools")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("zpools 为数组");
        assert!(!arr.is_empty());
        assert!(arr[0]["name"].is_string());
        assert!(arr[0]["healthy"].is_boolean());
    }

    #[tokio::test]
    async fn stats_returns_aggregated_summary() {
        let h = MonitorRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/monitor/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["cpu_usage"].is_number());
        assert!(resp.body["cpu_cores"].as_u64().unwrap() >= 1);
        assert!(resp.body["alerts_total"].as_u64().unwrap() >= 1);
        assert!(resp.body["zpools_total"].as_u64().unwrap() >= 1);
        assert!(resp.body["hostname"].is_string());
    }

    #[test]
    fn parse_size_to_bytes_units() {
        assert_eq!(parse_size_to_bytes("0"), 0);
        assert_eq!(parse_size_to_bytes("-"), 0);
        assert_eq!(parse_size_to_bytes(""), 0);
        assert_eq!(parse_size_to_bytes("928G"), 928 * 1024u64.pow(3));
        assert_eq!(parse_size_to_bytes("2T"), 2 * 1024u64.pow(4));
        assert_eq!(parse_size_to_bytes("1.5G"), (1.5 * 1024f64.powi(3)) as u64);
    }

    #[test]
    fn parse_zpool_line_parses() {
        let line = "tank   928G   612K   928G   -   -   0%   0%   1.00x   ONLINE   -";
        let pool = parse_zpool_line(line).unwrap();
        assert_eq!(pool.name, "tank");
        assert_eq!(pool.state, "ONLINE");
        assert!(pool.healthy);
        assert_eq!(pool.size_bytes, 928 * 1024u64.pow(3));
    }

    #[test]
    fn parse_zpool_line_rejects_short() {
        assert!(parse_zpool_line("short").is_none());
        assert!(parse_zpool_line("").is_none());
    }

    #[test]
    fn meminfo_value_parsing() {
        assert_eq!(parse_first_kb("   16384000 kB"), 16384000);
        assert_eq!(parse_first_kb("  abc kB"), 0);
        assert_eq!(parse_first_kb(""), 0);
    }

    #[test]
    fn cpu_usage_delta_returns_zero_on_first_call() {
        let h = MonitorRouteHandler::new();
        let sample = CpuSample {
            total: 1000,
            idle: 500,
        };
        let usage = h.cpu_usage_delta(sample);
        assert_eq!(usage, 0.0, "首次调用应返回 0（无上次采样）");
    }

    #[test]
    fn cpu_usage_delta_computes_on_second_call() {
        let h = MonitorRouteHandler::new();
        let s1 = CpuSample {
            total: 1000,
            idle: 500,
        };
        let s2 = CpuSample {
            total: 2000,
            idle: 800,
        };
        let _ = h.cpu_usage_delta(s1); // 首次记录
        let usage = h.cpu_usage_delta(s2);
        // busy_delta = (2000-800) - (1000-500) = 1200-500 = 700
        // total_delta = 2000-1000 = 1000
        // usage = 700/1000 = 70%
        assert!(
            (usage - 70.0).abs() < 0.1,
            "second-call usage ~70%, got {usage}"
        );
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<MonitorRouteHandler>();
    }

    // —— 新增：SQLite 持久化 roundtrip / ack 更新 / 阈值纯函数 ——
    fn sample_metrics(cpu: f32, mem_ratio: f32, disk_ratio: f32) -> SystemMetrics {
        let mem_total = 10_000_000_000u64;
        let mem_used = (mem_total as f32 * mem_ratio) as u64;
        let disk_total = 1_000_000_000_000u64;
        let disk_used = (disk_total as f32 * disk_ratio) as u64;
        SystemMetrics {
            hostname: "test-host".into(),
            uptime_secs: 100,
            load_avg: [0.1, 0.2, 0.3],
            cpu_usage: cpu,
            cpu_cores: 2,
            mem_total_bytes: mem_total,
            mem_used_bytes: mem_used,
            mem_available_bytes: mem_total - mem_used,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            disk_total_bytes: disk_total,
            disk_used_bytes: disk_used,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            processes: 10,
            kernel_version: "test".into(),
        }
    }

    #[tokio::test]
    async fn alerts_sqlite_roundtrip_persists_and_lists() {
        let h = MonitorRouteHandler::with_empty();
        // 初始空
        assert!(h.alerts_snapshot().is_empty());
        // 插入一条（经由内部方法，确保走 SQLite）
        let a = h.insert_alert_internal("warning", "测试告警 A", "cpu");
        // 列表能看到，且字段 roundtrip 一致
        let snap = h.alerts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, a.id);
        assert_eq!(snap[0].level, "warning");
        assert_eq!(snap[0].message, "测试告警 A");
        assert_eq!(snap[0].source, "cpu");
        assert!(!snap[0].acked);
        // GET /alerts 端点也走 SQLite
        let resp = h.handle(get_req("/api/v1/monitor/alerts")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], a.id);
    }

    #[tokio::test]
    async fn ack_updates_sqlite_acked_flag() {
        let h = MonitorRouteHandler::with_empty();
        let a = h.insert_alert_internal("critical", "磁盘满", "disk");
        // ack 之前未确认
        let before = h.alerts_snapshot();
        assert!(!before.iter().find(|x| x.id == a.id).unwrap().acked);
        // 调 ack 端点
        let resp = h
            .handle(post_req(
                &format!("/api/v1/monitor/alerts/{}/ack", a.id),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["acked"], true);
        // 经 SQLite 重读确认已落库
        let after = h.alerts_snapshot();
        let updated = after.iter().find(|x| x.id == a.id).unwrap();
        assert!(updated.acked);
        assert_eq!(updated.level, "critical");
    }

    #[test]
    fn check_thresholds_emits_for_cpu_mem_disk_violations() {
        // 三项均超阈值：cpu>85 / mem>0.90 / disk>0.90
        let m = sample_metrics(90.0, 0.95, 0.95);
        let alerts = check_thresholds(&m);
        let levels: Vec<&str> = alerts.iter().map(|a| a.source.as_str()).collect();
        assert!(levels.contains(&"cpu"), "cpu>85 应触发: {levels:?}");
        assert!(levels.contains(&"memory"), "mem>0.90 应触发: {levels:?}");
        assert!(levels.contains(&"disk"), "disk>0.90 应触发: {levels:?}");
        // cpu / disk 应 critical，memory 应 warning
        let cpu = alerts.iter().find(|a| a.source == "cpu").unwrap();
        assert_eq!(cpu.level, "critical");
        let mem = alerts.iter().find(|a| a.source == "memory").unwrap();
        assert_eq!(mem.level, "warning");
        let disk = alerts.iter().find(|a| a.source == "disk").unwrap();
        assert_eq!(disk.level, "critical");
    }

    #[test]
    fn check_thresholds_empty_when_all_normal() {
        let m = sample_metrics(10.0, 0.40, 0.30);
        let alerts = check_thresholds(&m);
        assert!(alerts.is_empty(), "全正常应无告警: {alerts:?}");
    }

    #[test]
    fn check_thresholds_boundary_cpu_85_not_triggered() {
        // 边界：cpu == 85.0 不应触发（严格 > 85）
        let m = sample_metrics(85.0, 0.50, 0.50);
        let alerts = check_thresholds(&m);
        assert!(alerts.iter().all(|a| a.source != "cpu"));
    }

    #[tokio::test]
    async fn sqlite_seed_demo_alerts_on_with_demo_data() {
        // with_demo_data 应预置 2 条 demo 告警（alert-1 / alert-2）
        let h = MonitorRouteHandler::with_demo_data();
        let snap = h.alerts_snapshot();
        assert_eq!(snap.len(), 2, "应预置 2 条 demo 告警: {snap:?}");
        let ids: Vec<&str> = snap.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"alert-1"));
        assert!(ids.contains(&"alert-2"));
        // 初始都未确认
        assert!(snap.iter().all(|a| !a.acked));
    }

    #[tokio::test]
    async fn dedup_blocks_recent_same_source_level() {
        let h = MonitorRouteHandler::with_empty();
        // 插一条 cpu+critical
        let _ = h.insert_alert_internal("critical", "CPU 高", "cpu");
        {
            let conn = h.db.lock().unwrap();
            // 同 source+level 5 分钟内 → 视为已存在（去重生效）
            assert!(recent_alert_exists(&conn, "cpu", "critical", 300));
            // 不同 level 不去重
            assert!(!recent_alert_exists(&conn, "cpu", "warning", 300));
            // 不同 source 不去重
            assert!(!recent_alert_exists(&conn, "disk", "critical", 300));
        }
    }
}
