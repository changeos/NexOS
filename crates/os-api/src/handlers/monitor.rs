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
//! | GET    | `/api/v1/monitor/vram`              | GPU 显存厂商/类型/结温（vraminfo，需 admin）|
//!
//! # 实时网速（`/net-rate`，2026-08-23）
//!
//! `/metrics` 的 `net_rx_bytes`/`net_tx_bytes` 是**开机以来的累计字节数**（单调
//! 计数器），不是速率——监控悬浮框此前把它当 B/s 展示导致"网速不对"。`/net-rate`
//! 读 `/proc/net/dev`（排除 `lo`），handler 内存态保存上次采样
//! （接口名 → (rx_bytes, tx_bytes) + 采样时刻），每次调用算差值得各接口与总计的
//! **字节/秒**速率；首次调用（无上次采样）返回全 0 并记录基线，下一轮差值生效。
//!
//! # 显存采集（`/vram`，v0.1.50）
//!
//! `nvidia-smi` 在 Linux 上不提供显存温度（开源内核模块报 `N/A`），显存颗粒厂商/
//! 类型也不在其公开字段里。本端点接入 **vraminfo**（MIT 单文件 C 工具，NexHub 仓
//! `vraminfo`，tag `v1.0.0-nexos`；上游 github.com/xzwgit/vraminfo）——探测式可选依赖，
//! ffmpeg/blender 同款契约：
//!
//! - **探测三态**：env `NEXOS_VRAMINFO_BIN`（可执行才认）→ `PATH` 扫描 → 常规落点
//!   （`/usr/local/bin/vraminfo` 等）；**缺失 → `{available:false, hint}`**，不报错、
//!   不影响监控其它部分（Cargo 零依赖，装了才多这块数据）。
//! - **采集**：单次 exec `vraminfo --json --per-module`（两 flag 合并一次拿全热点 +
//!   逐颗粒温度；GDDR7 才有逐颗粒测点），**5 秒超时**；结果**缓存 10 秒**（handler
//!   内存态，监控轮询不重复 spawn）。温度读数需 root——**提权自适应**：os-api 以
//!   root 跑时直接 exec；非 root 部署（106 `User=oem`）先 `sudo -n <bin>`
//!   （NOPASSWD 免交互，`/etc/sudoers.d/nexos-vraminfo` 仅授权 vraminfo 本体），
//!   sudo 层失败（未装/未授权/需密码）回落直接 exec、保留非 root note 降级。端点
//!   整体 admin 鉴权。
//! - **降级语义（诚实边界）**：GDDR6/GDDR5 颗粒**硬件上没有**显存温度传感器，工具在
//!   `note` 里如实说明（`no memory temperature sensor on this DRAM type`）——温度
//!   null + note 原样透传，**不告警**（硬件属性不是故障）；非 root / `iomem=relaxed`
//!   等失败形态的 note 同样透传。
//! - **告警**（接入既有阈值规则引擎，source=`vram`）：`memory_temp_c ≥
//!   NEXOS_VRAM_TEMP_WARN`（缺省 **100°C**）→ warning，`≥ NEXOS_VRAM_TEMP_CRIT`
//!   （缺省 **110°C**，硬件降频/保护点）→ critical；无传感器/工具缺失不参与判定。

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
    /// `cpu` / `memory` / `disk` / `service` / `vram`（显存温度，v0.1.50）。
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

/// 一块 GPU 的显存信息（`vraminfo --json --per-module` 单卡条目的透传子集；
/// 未提供的字段为 `None`/缺省，前端按降级语义渲染）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VramGpu {
    /// PCI 地址（如 `0000:02:00.0`）。
    #[serde(default)]
    pub bdf: String,
    /// 板卡厂商（子系统 ID 非 board partner 时 `None`，如 NVIDIA 公版参考 ID）。
    #[serde(default)]
    pub board_vendor: Option<String>,
    /// 显存颗粒厂商（Samsung / Hynix / Micron …；NVAPI 不可用时 `None`）。
    #[serde(default)]
    pub memory_maker: Option<String>,
    /// 显存类型（GDDR5 / GDDR6 / GDDR6X / GDDR7）。
    #[serde(default)]
    pub memory_type: Option<String>,
    /// 显存结温（°C）。GDDR6/GDDR5 无传感器 → `None` + `note` 说明。
    #[serde(default)]
    pub memory_temp_c: Option<i64>,
    /// 逐颗粒温度（°C；仅 GDDR7 多测点 + `--per-module` 时有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_temp_modules_c: Option<Vec<i64>>,
    /// 读不到温度时的原因说明（无传感器 / 需 root / 建议 iomem=relaxed …）。
    #[serde(default)]
    pub note: Option<String>,
}

/// `GET /api/v1/monitor/vram` 响应（探测式可选依赖，三态见模块文档）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VramSnapshot {
    /// 工具是否可用（探测命中且可执行）。
    pub available: bool,
    /// 命中的二进制路径（available 时必有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// 各 GPU 明细（available 时必有；exec 失败/超时为空数组 + error）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpus: Option<Vec<VramGpu>>,
    /// 工具缺失时的安装指引（available=false 时必有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 工具在但 exec/解析失败的原因（如无 NVIDIA GPU / 超时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 缓存条目年龄（秒；由响应组装时填，采集层恒 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
}

/// 显存采集缓存条目（采集时刻 + 快照；TTL 10s，监控轮询不重复 spawn）。
type VramCacheEntry = (std::time::Instant, VramSnapshot);

// ----------------------------------------------------------------------------
// MonitorRouteHandler
// ----------------------------------------------------------------------------

/// 系统监控路由处理器——HTTP 边界适配到真实系统指标 + SQLite 持久化告警。
///
/// 持有：
/// - `db: Arc<Mutex<Connection>>`（SQLite：alerts 表；短锁快查快放，不跨 `.await` 持锁）；
/// - `last_cpu: Mutex<Option<(Instant, CpuSample)>>`（两次 `/proc/stat` 采样算 CPU%）；
/// - `last_net: Mutex<Option<NetSample>>`
///   （两次 `/proc/net/dev` 采样算实时网速——`/net-rate` 端点内存态）；
/// - `counter: Mutex<u64>`（生成告警 id 的自增计数）；
/// - `vram_bin_override: Option<String>`（vraminfo 固定路径，测试注入 fake 脚本壳；
///   None=生产解析链 env `NEXOS_VRAMINFO_BIN` → PATH → 常规落点）；
/// - `vram_cache: Arc<Mutex<Option<VramCacheEntry>>>`（`/vram` 端点 10s 采集缓存；
///   `Arc` 供后台告警引擎 task 共享，引擎轮询与 HTTP 轮询共用同一份缓存）。
///
/// `db` 用 `Arc` 是为了把一个 clone 交给后台阈值规则引擎 task（`spawn_alert_engine`），
/// 该 task 与本 handler 共享同一 SQLite 文件（`vram_cache` 同理）。
pub struct MonitorRouteHandler {
    db: Arc<Mutex<Connection>>,
    last_cpu: Mutex<Option<(std::time::Instant, CpuSample)>>,
    last_net: Mutex<Option<NetSample>>,
    counter: Mutex<u64>,
    vram_bin_override: Option<String>,
    vram_cache: Arc<Mutex<Option<VramCacheEntry>>>,
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
            vram_bin_override: None,
            vram_cache: Arc::new(Mutex::new(None)),
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
            vram_bin_override: None,
            vram_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 测试注入：固定 vraminfo 路径（fake 脚本壳 / 不存在路径——缺失时端点走
    /// `{available:false, hint}` 降级；probe_utils 探测注入同款）。
    /// 覆写值**不落回**解析链（Some 即生效，可执行性由采集层过滤）。
    #[must_use]
    pub fn with_vram_bin(mut self, path: &str) -> Self {
        self.vram_bin_override = Some(path.to_string());
        self
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

    /// Monitor DB 短事务统一入口（DB 收敛第三批，2026-09-25，照抄 im.rs
    /// `im_db_call` 手法——审计 A1-1/Top1 续批）：闭包在 `spawn_blocking`
    /// 线程里拿锁执行——**锁等待与 SQLite 同步 IO 全部离开 tokio worker**，
    /// async 任务只 `.await` 结果。
    ///
    /// 锁语义与原先「async 里直接 `self.db.lock()`」完全一致：同一把组件级
    /// 互斥锁（monitor.db 单连接，与后台告警引擎 task 共享）、短锁快放不跨
    /// `.await`；闭包 panic 经 `resume_unwind` 原样上抛（等价原
    /// `expect("db poisoned")`）。
    async fn db_call<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> T + Send + 'static,
    {
        monitor_db_call(Arc::clone(&self.db), f).await
    }

    /// 当前告警快照（按时间倒序，最近 100 条）。第三批（2026-09-25）起
    /// async：查表在 [`Self::db_call`] 的 spawn_blocking 线程里拿锁执行。
    #[must_use]
    pub async fn alerts_snapshot(&self) -> Vec<Alert> {
        self.db_call(|conn| list_alerts(conn, 100)).await
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

    /// 显存快照（GET /api/v1/monitor/vram 的内核）：探测 vraminfo → 命中则
    /// exec `--json --per-module`（5s 超时）解析透传，结果缓存 10s；未命中降级
    /// `{available:false, hint}`。见模块文档"显存采集"节。
    async fn vram_snapshot(&self) -> VramSnapshot {
        // 覆写值短路解析链（Some 即生效）；可执行性过滤在解析内核/采集层做
        let bin = self
            .vram_bin_override
            .clone()
            .or_else(detect_vraminfo)
            .filter(|p| super::probe_utils::is_executable(p));
        vram_snapshot_shared(bin, &self.vram_cache).await
    }

    /// 插入一条告警（分配 id + 写 DB）。返回写入后的 Alert。
    /// （第三批起 async：写入在 [`Self::db_call`] 的 spawn_blocking 里。）
    #[allow(dead_code)]
    async fn insert_alert_internal(&self, level: &str, message: &str, source: &str) -> Alert {
        let alert = Alert {
            id: self.next_alert_id(),
            level: level.to_string(),
            message: message.to_string(),
            source: source.to_string(),
            timestamp: now_iso(),
            acked: false,
        };
        let alert_for_db = alert.clone();
        let level_for_log = level.to_string();
        let message_for_log = message.to_string();
        let source_for_log = source.to_string();
        self.db_call(move |conn| {
            // 插入失败不 panic（与上游降级语义一致），仅打 stderr
            if let Err(e) = insert_alert(conn, &alert_for_db) {
                eprintln!(
                    "monitor: 插入告警失败（{e}）：{level_for_log} {source_for_log} {message_for_log}"
                );
            }
        })
        .await;
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
        let vram_cache = Arc::clone(&self.vram_cache);
        let vram_bin_override = self.vram_bin_override.clone();
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
                // 5.5 显存温度阈值（v0.1.50，source=vram）：工具在且温度可读才参与
                //     判定（无传感器 note 形态 memory_temp_c=None 天然不触发）；
                //     与 HTTP /vram 端点共享 10s 采集缓存，不重复 spawn。
                let (warn_c, crit_c) = vram_temp_thresholds();
                let vram_bin = vram_bin_override
                    .clone()
                    .or_else(detect_vraminfo)
                    .filter(|p| super::probe_utils::is_executable(p));
                if vram_bin.is_some() {
                    let snap = vram_snapshot_shared(vram_bin, &vram_cache).await;
                    if snap.available {
                        candidates.extend(check_vram_thresholds(
                            snap.gpus.as_deref().unwrap_or(&[]),
                            warn_c,
                            crit_c,
                        ));
                    }
                }
                // 6. 去重 + 写库（同 source+level 5 分钟内不重复）
                // （DB 收敛第三批，2026-09-25：整段「查重 + 分配 id + 写入」在
                // `spawn_blocking` 线程里**单锁一次持锁**完成——与原先锁内
                // 连续处理全部候选的临界区一比一相同，原子/互斥语义不变；锁
                // 中毒沿旧容错：跳过本轮只记日志，引擎循环不死。）
                let db_for_round = Arc::clone(&db);
                let candidates_for_db = candidates.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || {
                    let conn_guard = match db_for_round.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            eprintln!("monitor: 引擎获取 db 锁失败（{e}），跳过本轮");
                            return;
                        }
                    };
                    for a in &candidates_for_db {
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
                })
                .await
                {
                    eprintln!("monitor: 引擎写库任务异常退出（{e}），本轮跳过");
                }
            }
        });
    }
}

/// Monitor DB 短事务统一入口（自由函数形态，供 [`MonitorRouteHandler::db_call`]
/// 与持 `Arc<Mutex<Connection>>` 的后台告警引擎 task 共用；手法与 im.rs
/// `im_db_call` 一致——spawn_blocking 里拿锁，锁等待与 SQLite IO 离开
/// tokio worker）。
async fn monitor_db_call<T, F>(db: Arc<Mutex<Connection>>, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let conn = db.lock().expect("db poisoned");
        f(&conn)
    })
    .await
    {
        Ok(v) => v,
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
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
            // 显存采集 admin：温度读数需 root（os-api root 直跑 exec），不公开
            spec(
                HttpMethod::Get,
                "/api/v1/monitor/vram",
                true,
                vec!["admin".into()],
            ),
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
                let alerts = self.alerts_snapshot().await;
                Ok(ok_json(to_value(&alerts)?))
            }

            // —— POST /api/v1/monitor/alerts/:id/ack —— 确认告警（SQLite UPDATE；
            //    第三批起查-写在 db_call 的 spawn_blocking 里单锁一次完成）
            (HttpMethod::Post, ["api", "v1", "monitor", "alerts", id, "ack"]) => {
                let id_for_db = id.to_string();
                let outcome = self
                    .db_call(move |conn| ack_alert(conn, &id_for_db).map_err(|e| e.to_string()))
                    .await;
                match outcome {
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

            // —— GET /api/v1/monitor/vram —— GPU 显存厂商/类型/结温（vraminfo，
            //    admin）：探测缺失 → {available:false, hint}；命中 → exec + 10s 缓存
            (HttpMethod::Get, ["api", "v1", "monitor", "vram"]) => {
                let snap = self.vram_snapshot().await;
                Ok(ok_json(to_value(&snap)?))
            }

            // —— GET /api/v1/monitor/stats —— 聚合摘要
            (HttpMethod::Get, ["api", "v1", "monitor", "stats"]) => {
                let metrics = read_metrics_blocking(self).await;
                let alerts = self.alerts_snapshot().await;
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
    let _ = conn.busy_timeout(std::time::Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
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

// ----------------------------------------------------------------------------
// vraminfo 显存探测与采集（v0.1.50；探测契约同 probe_utils 的 ffmpeg/blender 链）
// ----------------------------------------------------------------------------

/// vraminfo 缺失时的安装指引（`/vram` 降级响应与 docs/MONITOR.md 同文案）。
pub const VRAMINFO_INSTALL_HINT: &str = "vraminfo 未安装：从 NexHub 仓 vraminfo 克隆后\
make && sudo make install（git clone /tank/git-repos/vraminfo.git，或 GitHub xzwgit/vraminfo），\
或设 env NEXOS_VRAMINFO_BIN 指向已有二进制。详见 docs/MONITOR.md";

/// vraminfo 常规落点（PATH 扫描之外的兜底候选，按序探测）。
const VRAMINFO_COMMON_PATHS: [&str; 2] = ["/usr/local/bin/vraminfo", "/usr/bin/vraminfo"];

/// 采集缓存 TTL（秒）：监控页轮询间隔内复用同一份 exec 结果，不重复 spawn。
const VRAM_CACHE_TTL_SECS: u64 = 10;

/// 单次 exec 超时（秒）：单文件 C 工具毫秒级返回，5s 上限防挂死。
const VRAM_EXEC_TIMEOUT_SECS: u64 = 5;

/// vraminfo 解析内核（参数化，测试注入合成值，不读进程 env；
/// `detect_ffmpeg_with` 同口径）：env 覆写（可执行才认）→ PATH 目录扫描 →
/// 常规落点候选。
#[must_use]
pub fn detect_vraminfo_with(
    env_bin: Option<&str>,
    path_dirs: &[String],
    extra_candidates: &[&str],
) -> Option<String> {
    if let Some(b) = env_bin.map(str::trim).filter(|s| !s.is_empty()) {
        if super::probe_utils::is_executable(b) {
            return Some(b.to_string());
        }
    }
    for d in path_dirs {
        let cand = if d.ends_with('/') {
            format!("{d}vraminfo")
        } else {
            format!("{d}/vraminfo")
        };
        if super::probe_utils::is_executable(&cand) {
            return Some(cand.to_string());
        }
    }
    extra_candidates
        .iter()
        .find(|p| super::probe_utils::is_executable(p))
        .map(|p| (*p).to_string())
}

/// 请求路径的 vraminfo 解析链（env `NEXOS_VRAMINFO_BIN` → PATH → 常规落点）。
#[must_use]
pub fn detect_vraminfo() -> Option<String> {
    let path_dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    detect_vraminfo_with(
        std::env::var("NEXOS_VRAMINFO_BIN").ok().as_deref(),
        &path_dirs,
        &VRAMINFO_COMMON_PATHS,
    )
}

/// 直接 exec 的 argv（`vraminfo --json --per-module`；两 flag 合并，一次拿全
/// 热点 + 逐颗粒——GDDR6X 只有热点寄存器、GDDR7 才有逐颗粒测点，无测点时
/// 工具侧自会省略 `memory_temp_modules_c` 字段）。
#[must_use]
pub fn vraminfo_direct_argv(bin: &str) -> Vec<String> {
    vec![bin.to_string(), "--json".into(), "--per-module".into()]
}

/// sudo 提权尝试的 argv（`sudo -n <bin> --json --per-module`；`-n` 免交互——
/// 密码/授权缺失直接失败，绝不挂起等终端输入）。
#[must_use]
pub fn vraminfo_sudo_argv(bin: &str) -> Vec<String> {
    vec![
        "sudo".into(),
        "-n".into(),
        bin.to_string(),
        "--json".into(),
        "--per-module".into(),
    ]
}

/// 当前进程是否以 root（euid=0）运行——root 部署时无需 sudo 前缀。
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .map(|s| {
            s.lines()
                .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))
        })
        .unwrap_or(false)
}

/// 跑一次 vraminfo 尝试（argv 可为 sudo 前缀形态）。错误串带标签便于回落判定：
/// `timeout` / `spawn:<prog>: <io错>` / `exit:<stderr 或 code 说明>`。
async fn run_vraminfo_once(
    argv: &[String],
    timeout: std::time::Duration,
) -> Result<String, String> {
    let prog = argv.first().map(String::as_str).unwrap_or_default();
    let fut = tokio::process::Command::new(prog)
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    let out = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| format!("spawn:{prog}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("exit:vraminfo 异常退出（code={:?}）", out.status.code())
        } else {
            format!("exit:{stderr}")
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// sudo 尝试是否失败（纯函数，测试注入带标签错误串）——只有 **sudo 层**的失败
/// 才回落直接 exec；工具自身的结果（如 `no NVIDIA GPU found`）是真实回执，
/// 回落只会白跑一遍：
/// - `spawn:sudo…`：sudo 未安装（启动即败）；
/// - `exit:` 且 stderr 以 `sudo:` 开头（sudo 报错恒带此前缀：未授权/需密码/
///   命令找不到）或含 `a password is required`（`-n` 下 NOPASSWD 未配置）；
/// - `timeout` 不回落（直接 exec 大概率同样挂死，别再花一个超时窗口）。
#[must_use]
pub fn sudo_attempt_failed(err: &str) -> bool {
    if let Some(rest) = err.strip_prefix("spawn:") {
        return rest.starts_with("sudo");
    }
    if let Some(rest) = err.strip_prefix("exit:") {
        let r = rest.trim_start();
        return r.starts_with("sudo:") || r.contains("a password is required");
    }
    false
}

/// 带标签错误串 → 端点 `error` 字段文案（timeout/spawn 补上下文，exit 原样）。
fn humanize_vram_err(err: &str) -> String {
    if err == "timeout" {
        return format!("vraminfo 超时（>{VRAM_EXEC_TIMEOUT_SECS}s）");
    }
    if let Some(rest) = err.strip_prefix("spawn:") {
        return format!("vraminfo 启动失败: {rest}");
    }
    err.strip_prefix("exit:").unwrap_or(err).to_string()
}

/// exec vraminfo（提权自适应，v0.1.50 sudoers 配套）：
///
/// - 进程已是 root（os-api 以 root 部署）→ 直接 exec；
/// - 非 root → 先 `sudo -n <bin> --json --per-module`（NOPASSWD 免交互；
///   106 部署 `/etc/sudoers.d/nexos-vraminfo`：oem 仅授权 vraminfo 本体）；
///   **sudo 层失败**（未装/未授权/需密码）→ 回落直接 exec，保留"非 root 读
///   不到温度"的 note 降级语义（GDDR6X 温度读数需 root，见 docs/MONITOR.md）。
///
/// 成功返回 stdout；失败（超时/启动失败/非零退出，如无 NVIDIA GPU 时 rc=1）
/// 返回 stderr 或原因。
pub(crate) async fn exec_vraminfo(bin: &str) -> Result<String, String> {
    exec_vraminfo_with("sudo", bin).await
}

/// 参数化内核（`sudo_prog` 测试注入 fake 脚本壳；生产恒 `"sudo"`）。
async fn exec_vraminfo_with(sudo_prog: &str, bin: &str) -> Result<String, String> {
    let timeout = std::time::Duration::from_secs(VRAM_EXEC_TIMEOUT_SECS);
    if !running_as_root() {
        let mut sudo_argv = vraminfo_sudo_argv(bin);
        sudo_argv[0] = sudo_prog.to_string(); // 测试注入点：仅首参替换
        match run_vraminfo_once(&sudo_argv, timeout).await {
            Ok(out) => return Ok(out),
            Err(e) if sudo_attempt_failed(&e) => {
                eprintln!("monitor: vraminfo sudo 提权不可用（{e}），回落直接 exec");
            }
            Err(e) => return Err(humanize_vram_err(&e)),
        }
    }
    run_vraminfo_once(&vraminfo_direct_argv(bin), timeout)
        .await
        .map_err(|e| humanize_vram_err(&e))
}

/// 解析 vraminfo JSON（宽容：未知字段忽略、字段缺失/null → None/缺省、
/// gpus 数组缺省空；整体不是合法 JSON → None——调用方按 error 降级）。
#[must_use]
pub fn parse_vraminfo_json(stdout: &str) -> Option<Vec<VramGpu>> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        gpus: Vec<VramGpu>,
    }
    serde_json::from_str::<Raw>(stdout).ok().map(|r| r.gpus)
}

/// 单次采集（无缓存）：exec → 解析。工具在但失败（超时/无 GPU/不可解析）时
/// `available:true` + 空明细 + error——"工具在"与"数据拿到"是两回事，如实分开。
async fn collect_vram_with(bin: &str) -> VramSnapshot {
    let base = VramSnapshot {
        available: true,
        bin: Some(bin.to_string()),
        gpus: Some(Vec::new()),
        ..VramSnapshot::default()
    };
    match exec_vraminfo(bin).await {
        Ok(stdout) => match parse_vraminfo_json(&stdout) {
            Some(gpus) => VramSnapshot {
                gpus: Some(gpus),
                ..base
            },
            None => VramSnapshot {
                error: Some("vraminfo 输出不可解析".into()),
                ..base
            },
        },
        Err(e) => VramSnapshot {
            error: Some(e),
            ..base
        },
    }
}

/// 缓存新鲜度判定（纯函数，单测直接断言；TTL 内视为新鲜）。
fn vram_cache_fresh(
    entry: &Option<VramCacheEntry>,
    now: std::time::Instant,
    ttl: std::time::Duration,
) -> bool {
    match entry {
        Some((at, _)) => now.duration_since(*at) < ttl,
        None => false,
    }
}

/// 探测结果 → 快照（`/vram` 端点与后台告警引擎共用同一份缓存）：
/// - `bin = None`（探测缺失/不可执行）→ `{available:false, hint}` 降级不报错；
/// - 命中 → 缓存新鲜（TTL 10s）直接复用（补 `age_secs`）；否则 exec 采集回填。
///
/// 不跨 `.await` 持锁：慢路径先放锁再 exec，回来后仅当缓存仍过期才回填
/// （并发首批请求可能各自 exec 一次，无害——结果一致，下一轮起共享）。
async fn vram_snapshot_shared(
    bin: Option<String>,
    cache: &Arc<Mutex<Option<VramCacheEntry>>>,
) -> VramSnapshot {
    let Some(bin) = bin else {
        return VramSnapshot {
            available: false,
            hint: Some(VRAMINFO_INSTALL_HINT.to_string()),
            ..VramSnapshot::default()
        };
    };
    let ttl = std::time::Duration::from_secs(VRAM_CACHE_TTL_SECS);
    // 快路径：锁内查缓存（只 clone，不放 exec）
    {
        let slot = cache.lock().expect("vram cache poisoned");
        let now = std::time::Instant::now();
        if vram_cache_fresh(&slot, now, ttl) {
            if let Some((at, snap)) = slot.as_ref() {
                let mut s = snap.clone();
                s.age_secs = Some(now.duration_since(*at).as_secs());
                return s;
            }
        }
    }
    // 慢路径：放锁 exec，回来再回填（仍过期才写，不覆盖更新的并发结果）
    let fresh = collect_vram_with(&bin).await;
    let now = std::time::Instant::now();
    let mut slot = cache.lock().expect("vram cache poisoned");
    if !vram_cache_fresh(&slot, now, ttl) {
        *slot = Some((now, fresh));
    }
    slot.as_ref()
        .map(|(at, s)| {
            let mut s = s.clone();
            s.age_secs = Some(now.duration_since(*at).as_secs());
            s
        })
        .expect("慢路径回填后缓存必有值")
}

/// 显存温度阈值（缺省 warn=100°C / crit=110°C——100 是长期运行建议上限，
/// 110 是 GDDR6X/GDDR7 的硬件降频/保护点）。env 覆写：`NEXOS_VRAM_TEMP_WARN`
/// / `NEXOS_VRAM_TEMP_CRIT`。
fn vram_temp_thresholds() -> (i64, i64) {
    parse_vram_thresholds(
        std::env::var("NEXOS_VRAM_TEMP_WARN").ok().as_deref(),
        std::env::var("NEXOS_VRAM_TEMP_CRIT").ok().as_deref(),
    )
}

/// 阈值解析内核（纯函数）：单值非数字/超 40..=150 合理域回落缺省；
/// warn > crit（倒挂成死区）时把 crit 抬到 warn——critical 永不落后于 warning。
#[must_use]
pub fn parse_vram_thresholds(warn_raw: Option<&str>, crit_raw: Option<&str>) -> (i64, i64) {
    const WARN_DEFAULT: i64 = 100;
    const CRIT_DEFAULT: i64 = 110;
    let parse = |raw: Option<&str>, default: i64| -> i64 {
        match raw.map(str::trim).and_then(|s| s.parse::<i64>().ok()) {
            Some(v) if (40..=150).contains(&v) => v,
            _ => default,
        }
    };
    let warn = parse(warn_raw, WARN_DEFAULT);
    let mut crit = parse(crit_raw, CRIT_DEFAULT);
    if warn > crit {
        crit = warn;
    }
    (warn, crit)
}

/// 显存温度阈值纯函数：给定一次 `/vram` 采集的 GPU 明细，返回触发的候选告警
/// （source=`vram`；不含 id/timestamp，由引擎统一补）。
///
/// 语义（与 CPU/内存/磁盘规则同族）：
/// - 只对 `memory_temp_c` 可读的卡判定——无传感器（GDDR6/GDDR5，note 说明）、
///   非 root、MMIO 失败等形态温度为 None，**不告警**（硬件属性/环境问题，不是
///   温度事件；note 经端点如实展示，由人判断）；
/// - 全场最热卡 ≥ crit → critical 一条；否则最热达标卡 ≥ warn → warning 一条
///   （每轮至多一条：更高严重度覆盖，多卡不重复轰炸——引擎侧另有同
///   source+level 5 分钟去重兜底）。
#[must_use]
pub fn check_vram_thresholds(gpus: &[VramGpu], warn_c: i64, crit_c: i64) -> Vec<Alert> {
    let hottest = gpus
        .iter()
        .filter(|g| g.memory_temp_c.is_some())
        .max_by_key(|g| g.memory_temp_c.unwrap_or(i64::MIN));
    let Some(hottest) = hottest else {
        return Vec::new();
    };
    let t = hottest.memory_temp_c.unwrap_or(i64::MIN);
    let mut ctx = hottest.bdf.clone();
    if let Some(mt) = &hottest.memory_type {
        if !mt.is_empty() {
            ctx.push(' ');
            ctx.push_str(mt);
        }
    }
    if t >= crit_c {
        vec![Alert {
            id: String::new(),
            level: "critical".into(),
            message: format!("显存温度 {t}°C ≥ {crit_c}°C（{ctx}）——已达硬件降频/保护点"),
            source: "vram".into(),
            timestamp: String::new(),
            acked: false,
        }]
    } else if t >= warn_c {
        vec![Alert {
            id: String::new(),
            level: "warning".into(),
            message: format!("显存温度 {t}°C ≥ {warn_c}°C（{ctx}）——长期建议 ≤100°C"),
            source: "vram".into(),
            timestamp: String::new(),
            acked: false,
        }]
    } else {
        Vec::new()
    }
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
    async fn routes_declares_nine_endpoints() {
        let h = MonitorRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 9);
        assert!(routes.iter().all(|r| r.handler_component == "monitor"));
        // 写操作 + /vram（显存温度读数需 root）需 admin，其余只读 GET 公开
        for r in &routes {
            if r.method == HttpMethod::Post || r.path == "/api/v1/monitor/vram" {
                assert!(r.requires_auth, "{} 应鉴权", r.path);
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
        let before = h.alerts_snapshot().await;
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
        let after = h.alerts_snapshot().await;
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
        assert!(h.alerts_snapshot().await.is_empty());
        // 插入一条（经由内部方法，确保走 SQLite）
        let a = h.insert_alert_internal("warning", "测试告警 A", "cpu").await;
        // 列表能看到，且字段 roundtrip 一致
        let snap = h.alerts_snapshot().await;
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
        let a = h.insert_alert_internal("critical", "磁盘满", "disk").await;
        // ack 之前未确认
        let before = h.alerts_snapshot().await;
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
        let after = h.alerts_snapshot().await;
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
        let snap = h.alerts_snapshot().await;
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
        let _ = h.insert_alert_internal("critical", "CPU 高", "cpu").await;
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

    // ==========================================================================
    // vraminfo 显存探测与采集（v0.1.50）：探测三态 / JSON 解析容错 / 缓存 /
    // 阈值边界 / 端点行为
    // ==========================================================================

    /// 测试用临时目录（进程隔离；unix 下 fake 脚本壳需要）。
    #[cfg(unix)]
    fn vram_tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("monitor-vram-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 写一个 fake vraminfo 脚本壳（unix；chmod 755）：
    /// `body` 为脚本主体（printf JSON / 计数 / 模拟失败等由调用方拼）。
    #[cfg(unix)]
    fn fake_vraminfo_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        fake_named_script(dir, "vraminfo", body)
    }

    /// 写一个指定名字的 fake 可执行脚本壳（unix；chmod 755）——fake vraminfo /
    /// fake sudo（模拟 NOPASSWD 通/不通）均用它。
    #[cfg(unix)]
    fn fake_named_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::create_dir_all(dir);
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// 构造 PATH 目录候选（探测内核参数化注入用）。
    fn dirs_of(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    // —— 探测三态（detect_vraminfo_with：env 覆写 / PATH 扫描 / 常规落点）——

    #[cfg(unix)]
    #[test]
    fn detect_vraminfo_env_override_wins_when_executable() {
        let dir = vram_tmp("env-hit");
        let bin = fake_vraminfo_script(&dir, "exit 0");
        let got = detect_vraminfo_with(Some(bin.to_str().unwrap()), &[], &[]);
        assert_eq!(got.as_deref(), Some(bin.to_str().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn detect_vraminfo_env_non_executable_falls_through_to_path() {
        let dir = vram_tmp("env-miss");
        let plain = dir.join("vraminfo-plain");
        std::fs::write(&plain, "not executable").unwrap(); // 无 x 位
        let bindir = dir.join("pathd");
        std::fs::create_dir_all(&bindir).unwrap();
        let hit = fake_vraminfo_script(&bindir, "exit 0");
        let got = detect_vraminfo_with(
            Some(plain.to_str().unwrap()),
            &dirs_of(&[bindir.to_str().unwrap()]),
            &[],
        );
        assert_eq!(
            got.as_deref(),
            Some(hit.to_str().unwrap()),
            "env 不可执行→PATH 兜底"
        );
    }

    #[cfg(unix)]
    #[test]
    fn detect_vraminfo_path_scan_and_common_fallback() {
        let dir = vram_tmp("path-common");
        let bindir = dir.join("pd");
        std::fs::create_dir_all(&bindir).unwrap();
        let hit = fake_vraminfo_script(&bindir, "exit 0");
        // PATH 命中
        assert_eq!(
            detect_vraminfo_with(None, &dirs_of(&[bindir.to_str().unwrap()]), &[],).as_deref(),
            Some(hit.to_str().unwrap())
        );
        // PATH 空 → 常规落点候选兜底
        let fake_common = dir.join("vraminfo-common");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake_common, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&fake_common, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            detect_vraminfo_with(
                None,
                &[],
                &[fake_common.to_str().unwrap(), "/nonexistent/vraminfo"],
            )
            .as_deref(),
            Some(fake_common.to_str().unwrap())
        );
        // 全空 → None（缺失态）
        assert_eq!(
            detect_vraminfo_with(None, &[], &["/nonexistent/vraminfo"]),
            None
        );
    }

    // —— JSON 解析容错（上游全字段 / note 各形态 / 容错降级）——

    /// 上游 `vraminfo --json --per-module` 真实形态样例（RTX 3090，
    /// 温度可读 note=null + GDDR7 逐颗粒）。
    const VRAM_JSON_FULL: &str = r#"{
  "gpus": [
    {
      "bdf": "0000:02:00.0",
      "device_id": "0x2204",
      "subsystem": "0x1043:0x87af",
      "board_vendor": "ASUSTeK Computer Inc.",
      "board_vendor_id": "0x1043",
      "memory_maker": "Micron",
      "memory_type": "GDDR6X",
      "memory_maker_id": 10,
      "memory_type_id": 15,
      "memory_temp_c": 52,
      "note": null
    },
    {
      "bdf": "0000:20:00.0",
      "device_id": "0x2b85",
      "subsystem": "0x10de:0x1613",
      "board_vendor": null,
      "board_vendor_id": "0x10de",
      "memory_maker": "Samsung",
      "memory_type": "GDDR7",
      "memory_maker_id": 1,
      "memory_type_id": 16,
      "memory_temp_c": 39,
      "memory_temp_modules_c": [38, 38, 36, 36, 38, 38, 36, 39],
      "note": null
    }
  ]
}"#;

    #[test]
    fn parse_vraminfo_json_full_passthrough() {
        let gpus = parse_vraminfo_json(VRAM_JSON_FULL).expect("完整样例必解析");
        assert_eq!(gpus.len(), 2);
        let g0 = &gpus[0];
        assert_eq!(g0.bdf, "0000:02:00.0");
        assert_eq!(g0.board_vendor.as_deref(), Some("ASUSTeK Computer Inc."));
        assert_eq!(g0.memory_maker.as_deref(), Some("Micron"));
        assert_eq!(g0.memory_type.as_deref(), Some("GDDR6X"));
        assert_eq!(g0.memory_temp_c, Some(52));
        assert_eq!(g0.note, None);
        assert!(g0.memory_temp_modules_c.is_none(), "GDDR6X 无逐颗粒字段");
        let g1 = &gpus[1];
        assert_eq!(
            g1.memory_temp_modules_c,
            Some(vec![38, 38, 36, 36, 38, 38, 36, 39]),
            "GDDR7 逐颗粒透传"
        );
        assert_eq!(g1.board_vendor, None, "公版参考 ID → null 透传");
    }

    #[test]
    fn parse_vraminfo_json_note_forms_temp_null() {
        // 3060 语义：GDDR6 无传感器 → note 说明、温度 null（3060 机器不在，
        // 单测覆盖该分支）
        let no_sensor = r#"{"gpus":[{"bdf":"0000:01:00.0","memory_maker":"Samsung",
            "memory_type":"GDDR6","memory_temp_c":null,
            "note":"no memory temperature sensor on this DRAM type"}]}"#;
        let g = &parse_vraminfo_json(no_sensor).unwrap()[0];
        assert_eq!(g.memory_temp_c, None);
        assert!(g
            .note
            .as_deref()
            .unwrap()
            .contains("no memory temperature sensor"));
        // 非 root 形态
        let not_root = r#"{"gpus":[{"bdf":"0000:01:00.0","memory_temp_c":null,
            "note":"open(/dev/mem) failed: Permission denied (run as root)"}]}"#;
        let g = &parse_vraminfo_json(not_root).unwrap()[0];
        assert_eq!(g.memory_temp_c, None);
        assert!(g.note.as_deref().unwrap().contains("run as root"));
        // 内核策略形态
        let iomem = r#"{"gpus":[{"bdf":"0000:01:00.0","memory_temp_c":null,
            "note":"MMIO read failed (try kernel parameter iomem=relaxed)"}]}"#;
        let g = &parse_vraminfo_json(iomem).unwrap()[0];
        assert!(g.note.as_deref().unwrap().contains("iomem=relaxed"));
    }

    #[test]
    fn parse_vraminfo_json_tolerant_degradation() {
        // 整体非 JSON / 空输出 → None（调用方按 error 降级）
        assert_eq!(parse_vraminfo_json(""), None);
        assert_eq!(parse_vraminfo_json("not json at all"), None);
        assert_eq!(parse_vraminfo_json("{\"gpus\": [}, broken"), None);
        // 缺 gpus 字段 → 空数组（Some——工具在、只是没卡明细）
        assert_eq!(parse_vraminfo_json("{}").map(|v| v.len()), Some(0));
        // 单卡字段全缺 → 缺省值条目（不 panic）
        let g = &parse_vraminfo_json(r#"{"gpus":[{}]}"#).unwrap()[0];
        assert_eq!(g.bdf, "");
        assert_eq!(g.memory_temp_c, None);
        assert_eq!(g.note, None);
        // 未知字段忽略、整型温度
        let g = &parse_vraminfo_json(
            r#"{"gpus":[{"bdf":"0000:03:00.0","future_field":true,"memory_temp_c":88}]}"#,
        )
        .unwrap()[0];
        assert_eq!(g.memory_temp_c, Some(88));
    }

    // —— 缓存（纯函数：TTL 内新鲜 / 过期重采）——

    #[test]
    fn vram_cache_freshness_boundaries() {
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(VRAM_CACHE_TTL_SECS);
        let snap = VramSnapshot {
            available: true,
            bin: Some("x".into()),
            gpus: Some(Vec::new()),
            ..VramSnapshot::default()
        };
        // 无缓存 → 不新鲜（须采集）
        assert!(!vram_cache_fresh(&None, now, ttl));
        // 刚采 → 新鲜
        assert!(vram_cache_fresh(&Some((now, snap.clone())), now, ttl));
        // 超 TTL → 过期
        let stale_at = now
            .checked_sub(ttl + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!vram_cache_fresh(&Some((stale_at, snap)), now, ttl));
    }

    // —— 阈值解析（env 覆写 / 非法回落 / 倒挂修正）——

    #[test]
    fn parse_vram_thresholds_defaults_and_overrides() {
        assert_eq!(
            parse_vram_thresholds(None, None),
            (100, 110),
            "缺省 100/110"
        );
        assert_eq!(parse_vram_thresholds(Some("95"), Some("105")), (95, 105));
        // 非数字 / 越界 → 该项回落缺省
        assert_eq!(parse_vram_thresholds(Some("abc"), Some("-5")), (100, 110));
        assert_eq!(parse_vram_thresholds(Some("200"), None), (100, 110));
        assert_eq!(
            parse_vram_thresholds(Some(" 98 "), Some("108")),
            (98, 108),
            "容忍空白"
        );
        // 倒挂（warn > crit）→ crit 抬到 warn，不留死区
        assert_eq!(parse_vram_thresholds(Some("115"), Some("100")), (115, 115));
    }

    // —— 阈值规则（边界 99/100/109/110 + 无传感器不告警 + 最热覆盖）——

    fn gpu_with_temp(bdf: &str, t: Option<i64>, mem_type: &str) -> VramGpu {
        VramGpu {
            bdf: bdf.into(),
            memory_type: Some(mem_type.into()),
            memory_temp_c: t,
            ..VramGpu::default()
        }
    }

    #[test]
    fn check_vram_thresholds_boundaries() {
        // 99 → 不告警
        assert!(check_vram_thresholds(
            &[gpu_with_temp("0000:02:00.0", Some(99), "GDDR6X")],
            100,
            110
        )
        .is_empty());
        // 100 → warning（≥ 边界含）
        let a = check_vram_thresholds(
            &[gpu_with_temp("0000:02:00.0", Some(100), "GDDR6X")],
            100,
            110,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].level, "warning");
        assert_eq!(a[0].source, "vram");
        assert!(a[0].message.contains("100°C"));
        assert!(a[0].message.contains("0000:02:00.0"));
        assert!(a[0].message.contains("GDDR6X"));
        // 109 → warning
        let a = check_vram_thresholds(&[gpu_with_temp("b", Some(109), "GDDR7")], 100, 110);
        assert_eq!(a[0].level, "warning");
        // 110 → critical（≥ 边界含）
        let a = check_vram_thresholds(&[gpu_with_temp("b", Some(110), "GDDR7")], 100, 110);
        assert_eq!(a[0].level, "critical");
        // 自定义阈值生效
        let a = check_vram_thresholds(&[gpu_with_temp("b", Some(95), "GDDR6X")], 90, 120);
        assert_eq!(a[0].level, "warning");
    }

    #[test]
    fn check_vram_thresholds_no_sensor_never_alerts() {
        // 3060 语义：无传感器（temp None + note）→ 不告警（硬件属性不是故障）
        let g3060 = VramGpu {
            bdf: "0000:01:00.0".into(),
            memory_maker: Some("Samsung".into()),
            memory_type: Some("GDDR6".into()),
            memory_temp_c: None,
            note: Some("no memory temperature sensor on this DRAM type".into()),
            ..VramGpu::default()
        };
        assert!(check_vram_thresholds(&[g3060], 100, 110).is_empty());
        // 空列表同理
        assert!(check_vram_thresholds(&[], 100, 110).is_empty());
    }

    #[test]
    fn check_vram_thresholds_hottest_wins_one_alert() {
        // 两卡：105（warning 档）+ 111（critical 档）→ 仅 critical 一条（最热覆盖）
        let gpus = vec![
            gpu_with_temp("0000:01:00.0", Some(105), "GDDR6X"),
            gpu_with_temp("0000:02:00.0", Some(111), "GDDR7"),
        ];
        let a = check_vram_thresholds(&gpus, 100, 110);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].level, "critical");
        assert!(
            a[0].message.contains("0000:02:00.0"),
            "最热卡入消息: {}",
            a[0].message
        );
        // 两卡都 warning 档 → 一条 warning
        let gpus = vec![
            gpu_with_temp("0000:01:00.0", Some(101), "GDDR6X"),
            gpu_with_temp("0000:02:00.0", Some(103), "GDDR6X"),
        ];
        let a = check_vram_thresholds(&gpus, 100, 110);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].level, "warning");
        assert!(a[0].message.contains("103"));
    }

    // —— 端点行为（handler 级：缺失降级 / 透传 / 缓存不重复 exec / 失败如实）——

    #[tokio::test]
    async fn vram_endpoint_missing_tool_degrades_with_hint() {
        // 覆写指向不存在路径（Some 即生效，短路解析链）→ available:false + hint
        let h = MonitorRouteHandler::with_empty().with_vram_bin("/nonexistent/vraminfo-xyz");
        let resp = h.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(resp.status, 200, "缺失是降级不是错误");
        assert_eq!(resp.body["available"], false);
        let hint = resp.body["hint"].as_str().unwrap();
        assert!(hint.contains("vraminfo 未安装"), "{hint}");
        assert!(hint.contains("NexHub"), "{hint}");
        assert!(resp.body.get("gpus").is_none(), "缺失态不带 gpus 字段");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vram_endpoint_serves_json_and_caches() {
        let dir = vram_tmp("serve-cache");
        // 计数脚本：每次 exec 追加一行到 calls.log 再吐 3090 JSON
        let calls = dir.join("calls.log");
        let json_52 = r#"{"gpus":[{"bdf":"0000:02:00.0","board_vendor":"ASUSTeK Computer Inc.","memory_maker":"Micron","memory_type":"GDDR6X","memory_temp_c":52,"memory_temp_modules_c":[50,52,54],"note":null}]}"#;
        let bin = fake_vraminfo_script(
            &dir,
            &format!("echo x >> '{}'\nprintf '%s' '{}'", calls.display(), json_52),
        );
        let h = MonitorRouteHandler::with_empty().with_vram_bin(bin.to_str().unwrap());
        // 第一次：exec 一次，字段透传
        let r1 = h.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body["available"], true);
        assert_eq!(r1.body["bin"], serde_json::json!(bin.to_str().unwrap()));
        assert_eq!(r1.body["gpus"][0]["memory_temp_c"], 52);
        assert_eq!(r1.body["gpus"][0]["memory_maker"], "Micron");
        assert_eq!(r1.body["gpus"][0]["memory_type"], "GDDR6X");
        assert_eq!(
            r1.body["gpus"][0]["memory_temp_modules_c"],
            serde_json::json!([50, 52, 54]),
            "逐颗粒透传"
        );
        assert_eq!(r1.body["gpus"][0]["note"], serde_json::Value::Null);
        assert_eq!(r1.body["age_secs"], 0, "采集当拍 age=0");
        // 第二次（TTL 内）：复用缓存，不重复 exec
        let r2 = h.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(r2.body["gpus"][0]["memory_temp_c"], 52);
        let lines = std::fs::read_to_string(&calls).unwrap().lines().count();
        assert_eq!(lines, 1, "10s TTL 内两次请求只 exec 一次: {lines}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vram_endpoint_tool_failure_reported_honestly() {
        let dir = vram_tmp("fail");
        // rc=1 + stderr（无 NVIDIA GPU 形态）→ available:true + error 如实
        let bin = fake_vraminfo_script(&dir, "echo 'no NVIDIA GPU found' >&2\nexit 1");
        let h = MonitorRouteHandler::with_empty().with_vram_bin(bin.to_str().unwrap());
        let resp = h.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["available"], true, "工具在，只是没数据");
        assert_eq!(resp.body["gpus"].as_array().unwrap().len(), 0);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("no NVIDIA GPU found"));
        // rc=0 但输出不可解析 → error=不可解析
        let dir2 = vram_tmp("garbage");
        let bin2 = fake_vraminfo_script(&dir2, "printf '%s' 'not-json'");
        let h2 = MonitorRouteHandler::with_empty().with_vram_bin(bin2.to_str().unwrap());
        let resp = h2.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(resp.body["available"], true);
        assert_eq!(resp.body["gpus"].as_array().unwrap().len(), 0);
        assert!(resp.body["error"].as_str().unwrap().contains("不可解析"));
    }

    #[test]
    fn vram_snapshot_shape_roundtrips_serde() {
        // 响应 DTO 序列化形态契约（前端/i18n 依赖字段名稳定性）
        let snap = VramSnapshot {
            available: false,
            hint: Some(VRAMINFO_INSTALL_HINT.to_string()),
            ..VramSnapshot::default()
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["available"], false);
        assert!(v["hint"].is_string());
        assert!(v.get("bin").is_none(), "None 字段不占位");
        assert!(v.get("age_secs").is_none());
        let back: VramSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back, snap);
    }

    // —— 真机验收（--ignored；CI 无 GPU/工具，默认跳过）——
    //
    // 106（RTX 3090，/usr/local/bin/vraminfo 已装）跑法——用已编译的测试二进制
    // 以 root 直跑（cargo 不进 sudo，避免 root-owned target 产物）：
    //   B=$(ls target/debug/deps/os_api-* | grep -v '\\.d$' | head -1)
    //   sudo "$B" --exact handlers::monitor::tests::vram_real_machine_root_exec \
    //        --ignored --nocapture
    // 非 root 跑也允许：106 已配 /etc/sudoers.d/nexos-vraminfo（oem NOPASSWD 仅授权
    // vraminfo 本体）——sudo 提权路径应读到真实温度；未配 sudoers 的机器回落直接
    // exec，温度降级为 note（两态都算链路通过，见 docs/MONITOR.md §1.1）。

    #[tokio::test]
    #[ignore = "真机验收：需本机装 vraminfo + NVIDIA 卡（106）；root 或 sudoers 任一可读温度"]
    async fn vram_real_machine_root_exec() {
        let h = MonitorRouteHandler::with_empty(); // 无覆写：走 env→PATH→常规落点
        let resp = h.handle(get_req("/api/v1/monitor/vram")).await.unwrap();
        assert_eq!(resp.status, 200);
        eprintln!(
            "vram snapshot: {}",
            serde_json::to_string_pretty(&resp.body).unwrap()
        );
        assert_eq!(
            resp.body["available"], true,
            "本机应探测到 vraminfo: {resp:?}"
        );
        assert!(resp.body["bin"].as_str().unwrap().contains("vraminfo"));
        let gpus = resp.body["gpus"].as_array().expect("gpus 数组");
        assert!(!gpus.is_empty(), "真机应有 NVIDIA GPU");
        let g = &gpus[0];
        assert!(
            g["memory_type"]
                .as_str()
                .is_some_and(|t| t.starts_with("GDDR")),
            "NVAPI 显存类型（免 root 段）: {g}"
        );
        assert!(g["memory_maker"].is_string(), "NVAPI 颗粒厂商: {g}");
        if g["memory_temp_c"].is_u64() {
            // root 直跑或 sudo NOPASSWD 提权：GDDR6X/7 结温真实读数
            eprintln!(
                "（温度读数到位：{}°C —— {}）",
                g["memory_temp_c"],
                if running_as_root() {
                    "root 直跑"
                } else {
                    "sudo -n 提权"
                }
            );
        } else {
            // 未配 sudoers 的非 root 部署：如实降级为 note（诚实边界）
            eprintln!("（温度降级为 note：sudoers 未配/非 root）");
            assert!(
                g["memory_temp_c"].is_null(),
                "非 root 温度应如实为 null: {g}"
            );
            assert!(g["note"].as_str().is_some_and(|n| !n.is_empty()));
        }
    }

    // —— sudo 提权路径（v0.1.50 sudoers 配套）：argv 形态 / 失败判定 / 成功与回落 ——

    #[test]
    fn vraminfo_argv_direct_and_sudo_forms() {
        assert_eq!(
            vraminfo_direct_argv("/usr/local/bin/vraminfo"),
            vec!["/usr/local/bin/vraminfo", "--json", "--per-module"]
        );
        assert_eq!(
            vraminfo_sudo_argv("/usr/local/bin/vraminfo"),
            vec![
                "sudo",
                "-n",
                "/usr/local/bin/vraminfo",
                "--json",
                "--per-module"
            ],
            "sudo 恒带 -n（免交互，缺授权直接失败不挂起）"
        );
    }

    #[test]
    fn sudo_attempt_failed_classification() {
        // sudo 层失败 → 回落
        assert!(
            sudo_attempt_failed("spawn:sudo: No such file or directory"),
            "sudo 未安装"
        );
        assert!(
            sudo_attempt_failed("exit:sudo: a password is required"),
            "NOPASSWD 未配置"
        );
        assert!(sudo_attempt_failed(
            "exit:sudo: oem is not allowed to execute /usr/local/bin/vraminfo as root"
        ));
        assert!(sudo_attempt_failed(
            "exit:sudo: vraminfo: command not found"
        ));
        // 工具自身结果 → 不回落（真实回执，回落只是白跑一遍）
        assert!(!sudo_attempt_failed("exit:no NVIDIA GPU found"));
        assert!(!sudo_attempt_failed("exit:cannot enumerate PCI devices"));
        assert!(!sudo_attempt_failed(
            "exit:vraminfo 异常退出（code=Some(1)）"
        ));
        // 非 sudo 的启动失败 / 超时 → 不回落（直接 exec 大概率同样结果）
        assert!(!sudo_attempt_failed(
            "spawn:/usr/local/bin/vraminfo: Permission denied"
        ));
        assert!(!sudo_attempt_failed("timeout"));
    }

    /// exec_vraminfo_with 的 e2e（unix fake 脚本壳）：fake sudo 直接吐
    /// **只有 sudo 路径才会出现**的 JSON（temp=77），证明走的是提权 argv；
    /// fake vraminfo 本体输出 temp=52。root 环境跑测试时跳过（sudo 路径不启用）。
    #[cfg(unix)]
    #[tokio::test]
    async fn vram_exec_takes_sudo_path_when_available() {
        if running_as_root() {
            eprintln!("（root 环境：sudo 路径不启用，跳过）");
            return;
        }
        let dir = vram_tmp("sudo-ok");
        let vram = fake_vraminfo_script(
            &dir,
            "printf '%s' '{\"gpus\":[{\"bdf\":\"b\",\"memory_temp_c\":52}]}'",
        );
        // fake sudo：忽略参数直接产出 sudo 路径专属 JSON（temp=77）
        let sudo_bin = fake_named_script(
            &dir.join("sudo-home"),
            "sudo",
            "printf '%s' '{\"gpus\":[{\"bdf\":\"b\",\"memory_temp_c\":77}]}'",
        );
        let out = exec_vraminfo_with(sudo_bin.to_str().unwrap(), vram.to_str().unwrap())
            .await
            .unwrap();
        let g = &parse_vraminfo_json(&out).unwrap()[0];
        assert_eq!(
            g.memory_temp_c,
            Some(77),
            "命中 sudo 提权路径（非直接 exec 的 52）"
        );
    }

    /// sudo 层失败（`-n` 需密码形态）→ 回落直接 exec，拿到本体输出（temp=52）。
    #[cfg(unix)]
    #[tokio::test]
    async fn vram_exec_falls_back_to_direct_when_sudo_unavailable() {
        if running_as_root() {
            eprintln!("（root 环境：sudo 路径不启用，跳过）");
            return;
        }
        let dir = vram_tmp("sudo-fallback");
        let vram = fake_vraminfo_script(
            &dir,
            "printf '%s' '{\"gpus\":[{\"bdf\":\"b\",\"memory_temp_c\":52}]}'",
        );
        // fake sudo：模拟 NOPASSWD 未配置（sudo -n 的真实报错形态，rc=1）
        let sudo_bin = fake_named_script(
            &dir.join("sudo-home2"),
            "sudo",
            "echo 'sudo: a password is required' >&2\nexit 1",
        );
        let out = exec_vraminfo_with(sudo_bin.to_str().unwrap(), vram.to_str().unwrap())
            .await
            .unwrap();
        let g = &parse_vraminfo_json(&out).unwrap()[0];
        assert_eq!(
            g.memory_temp_c,
            Some(52),
            "sudo 失败后回落直接 exec 拿到本体输出"
        );
    }
}
