//! `ForwardingRouteHandler` —— 远程转发工具（SSH 隧道 + Windows RDP 转发）的
//! HTTP REST 入口（规划文档 §3.6 / §9.1#10）。
//!
//! 定位：让 Web UI / CLI / MCP 能经 HTTP 管理两类端口转发——
//! - **SSH 隧道**（`ssh/*`）：spawn 系统 `ssh` 子进程做 local / remote / dynamic
//!   三种隧道，**密钥认证红线**（`BatchMode=yes` 禁密码交互，模型里无任何密码
//!   字段，请求体出现 `password` 直接 400）；
//! - **RDP 转发**（`rdp/*`）：纯 Rust TCP 代理（tokio `TcpListener` +
//!   `copy_bidirectional`）把本机 `listen_port` 转发到远端 Windows RDP，
//!   并生成 `.rdp` 客户端配置文件下载。
//!
//! # 实现策略：SQLite 持久化 + 子进程/TCP 代理编排
//!
//! 隧道/转发定义全部落 SQLite（`Mutex<Connection>` 短锁快查快放，参考
//! [`crate::handlers::im`] / [`crate::handlers::api_gateway`] 的模式），重启后
//! 定义保留。运行态分两层：
//! - **SSH**：`start` spawn `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes
//!   -o ServerAliveInterval=30 -o StrictHostKeyChecking=accept-new -i <key> -p <port>`
//!   再加模式参数（`-L` / `-R` / `-D`），spawn 后 ~800ms 探测退出码——起不来立刻
//!   `failed` 带 stderr 摘要（stderr 落临时日志文件，避免管道满阻塞），存活则
//!   `running` + pid。子进程 `Child` 句柄存内存表（stop 时精确 kill），重启后
//!   经 `kill -0` 存活探测收养旧 pid 或降级 stopped；
//! - **RDP**：`start` 绑定 `0.0.0.0:<listen_port>`，每连接 `TcpStream::connect`
//!   远端 + `copy_bidirectional` 双向拷贝，accept 计数（累计连接数持久化）；
//!   端口冲突/占用降级 `error` 状态带原因，绝不 panic。
//!
//! `ssh` 二进制路径可用环境变量 `NEXOS_SSH_BIN` 覆写（默认 `ssh`）——测试注入
//! `/bin/false`（必失败）或 shell 脚本（模拟存活），不起真实网络。
//!
//! # 韧性 watchdog（2026-08-20 无人值守批次）
//!
//! `watchdog=true` 的 SSH 隧道由后台任务看护（`spawn_watchdog`，随
//! `spawn_autostart_resume` 一起启动）：每 `NEXOS_FORWARDING_WATCHDOG_SECS` 秒
//! （默认 30）全量扫一遍 tunnels——状态 `running` 但 ssh 进程已死（本进程
//! `Child::try_wait` 权威判定 / 收养 pid 走 `kill -0`）→ 自动重试 `start`。
//! 重试带放弃阈值：连续失败 5 次（内存计数，成功即清零）→ 置 `failed` +
//! error="watchdog 放弃：连续失败…"，直到用户手动 start 才会再被看护。
//! 用户手动 stop（状态 stopped）永远不拉起。每次成功拉起计数持久化
//! （`restart_count` / `last_restart_at`，详情响应可见）。
//!
//! 与 autostart 的分工：autostart 只在 **os-api 启动时** 恢复一次（进程拉起
//! 时机 = 服务重启）；watchdog 管 **运行期间** ssh 进程意外死亡的自愈（时机 =
//! 每个扫描 tick）。两者独立开关、可叠加。
//!
//! 不 seed demo 数据：这是真实工具配置（autostart 隧道会在启动时真实 spawn
//! ssh / 绑定端口），预置假条目可能误占端口或误导用户，各 `GET` 首次返回空数组。
//!
//! # 路由表（13 条，component="forwarding"）
//!
//! | method | path                                          | 动作 |
//! |--------|-----------------------------------------------|------|
//! | GET    | `/api/v1/forwarding/ssh`                      | SSH 隧道列表 |
//! | POST   | `/api/v1/forwarding/ssh`                      | 创建隧道（admin；`watchdog` 可选）|
//! | GET    | `/api/v1/forwarding/ssh/:id`                 | 隧道详情（实时存活探测 + 重启计数）|
//! | DELETE | `/api/v1/forwarding/ssh/:id`                 | 删隧道（运行中先停，admin）|
//! | POST   | `/api/v1/forwarding/ssh/:id/start`           | 启动（spawn ssh，admin）|
//! | POST   | `/api/v1/forwarding/ssh/:id/stop`            | 停止（kill 子进程，admin）|
//! | GET    | `/api/v1/forwarding/rdp`                     | RDP 转发列表 |
//! | POST   | `/api/v1/forwarding/rdp`                     | 创建转发（admin）|
//! | DELETE | `/api/v1/forwarding/rdp/:id`                 | 删转发（运行中先停，admin）|
//! | POST   | `/api/v1/forwarding/rdp/:id/start`           | 启动 TCP 代理（admin）|
//! | POST   | `/api/v1/forwarding/rdp/:id/stop`            | 停止代理（admin）|
//! | GET    | `/api/v1/forwarding/rdp/:id/rdp-file?username=` | 下载 `.rdp` 客户端配置 |
//! | GET    | `/api/v1/forwarding/stats`                   | 两类总数/运行数聚合 |

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// SSH 隧道（ssh_tunnels 行）。
///
/// 密钥认证红线：**没有任何密码字段**——认证只经 `private_key_path` 指定的
/// 私钥（默认 `~/.ssh/id_ed25519`），spawn 时强制 `BatchMode=yes` 禁密码交互。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTunnel {
    pub id: String,
    pub name: String,
    /// SSH 服务器主机名/IP。
    pub ssh_host: String,
    /// SSH 端口（默认 22）。
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// SSH 用户名。
    pub ssh_user: String,
    /// 私钥路径；None = 默认 `~/.ssh/id_ed25519`（透传给 ssh -i，支持 ~ 展开）。
    #[serde(default)]
    pub private_key_path: Option<String>,
    /// `local`（-L 本地转发）/ `remote`（-R 远程转发）/ `dynamic`（-D SOCKS 动态）。
    #[serde(default = "default_mode_local")]
    pub mode: String,
    /// 本地绑定地址（`127.0.0.1:8080` / `0.0.0.0:8080`）；remote 模式下当作远端绑定。
    pub local_bind: String,
    /// 转发目标主机（local/remote 模式必填；dynamic 模式必须为空）。
    #[serde(default)]
    pub remote_host: Option<String>,
    /// 转发目标端口（local/remote 模式必填；dynamic 模式必须为空）。
    #[serde(default)]
    pub remote_port: Option<u16>,
    /// os-api 启动时自动拉起（resume_autostart）。
    #[serde(default)]
    pub autostart: bool,
    /// 运行期 watchdog 看护：ssh 进程意外死亡时自动重试拉起（连续失败 5 次
    /// 放弃标 failed）。与 autostart（仅启动时恢复一次）正交，可叠加。
    /// 存量兼容：老行/老请求体缺该字段 → serde default false。
    #[serde(default)]
    pub watchdog: bool,
    /// `stopped` / `running` / `failed`。
    #[serde(default = "default_status_stopped")]
    pub status: String,
    /// 运行中 ssh 子进程 pid。
    #[serde(default)]
    pub pid: Option<u32>,
    /// 最近一次错误摘要（spawn 失败 / 异常退出 / 停止残留）。
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: String,
    /// 最近一次成功启动时间（RFC3339，可空）。
    #[serde(default)]
    pub last_started: Option<String>,
    /// watchdog 自动拉起成功累计次数（持久化；手动 start 不计）。
    #[serde(default)]
    pub restart_count: u32,
    /// 最近一次 watchdog 自动拉起成功时间（RFC3339，可空）。
    #[serde(default)]
    pub last_restart_at: Option<String>,
}

/// RDP 转发（rdp_forwards 行）——纯 Rust TCP 代理 + .rdp 文件生成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdpForward {
    pub id: String,
    pub name: String,
    /// 远端 Windows 主机（RDP 服务器）。
    pub target_host: String,
    /// 远端 RDP 端口（默认 3389）。
    #[serde(default = "default_rdp_port")]
    pub target_port: u16,
    /// 本机监听端口（0.0.0.0:<listen_port> → target）。
    pub listen_port: u16,
    /// os-api 启动时自动拉起。
    #[serde(default)]
    pub autostart: bool,
    /// `running` / `stopped` / `error`。
    #[serde(default = "default_status_stopped")]
    pub status: String,
    /// 累计接受的连接数（持久化）。
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: String,
}

/// 转发统计（GET /api/v1/forwarding/stats 响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardingStats {
    pub ssh_tunnels_total: usize,
    pub ssh_tunnels_running: usize,
    pub rdp_forwards_total: usize,
    pub rdp_forwards_running: usize,
    /// RDP 转发累计连接数（所有转发之和）。
    pub rdp_total_connections: u64,
}

// ----------------------------------------------------------------------------
// ForwardingRouteHandler
// ----------------------------------------------------------------------------

/// 远程转发路由处理器——SSH 隧道（spawn 系统 ssh 子进程）+ RDP 转发
/// （纯 Rust TCP 代理）+ .rdp 文件生成。
///
/// 状态三层（全部 `Arc` 共享，短锁快放，不跨 `.await` 持锁）：
/// - `db`：SQLite 定义/状态持久化（重启后隧道定义保留）；
/// - `children`：运行中的 ssh `Child` 句柄（stop 精确 kill）；
/// - `proxies`：运行中的 RDP accept loop `JoinHandle`（stop 时 abort）。
pub struct ForwardingRouteHandler {
    db: Arc<Mutex<Connection>>,
    /// tunnel id → 运行中的 ssh 子进程。
    children: Arc<Mutex<HashMap<String, tokio::process::Child>>>,
    /// rdp id → 运行中的 TCP 代理任务。
    proxies: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    /// tunnel id → watchdog 连续拉起失败计数（内存；成功清零，达
    /// [`WATCHDOG_MAX_FAILURES`] 放弃）。与 db/children 同款 Arc 共享，
    /// 后台 watchdog 任务与注册进网关的本体看同一份。
    watchdog_fails: Arc<Mutex<HashMap<String, u32>>>,
}

impl ForwardingRouteHandler {
    /// 构造 handler，打开默认 DB 路径 + 建表（不 seed，理由见模块头）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_db_path(&default_db_path())
    }

    /// 用指定 DB 路径构造（测试/诊断注入）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        let conn = open_db(path).unwrap_or_else(|e| {
            eprintln!("forwarding: 打开 SQLite {path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        Self {
            db: Arc::new(Mutex::new(conn)),
            children: Arc::new(Mutex::new(HashMap::new())),
            proxies: Arc::new(Mutex::new(HashMap::new())),
            watchdog_fails: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 用临时内存库构造（测试注入：数据隔离，进程结束即丢，无 seed）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self {
            db: Arc::new(Mutex::new(conn)),
            children: Arc::new(Mutex::new(HashMap::new())),
            proxies: Arc::new(Mutex::new(HashMap::new())),
            watchdog_fails: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 当前全量 SSH 隧道快照（从 DB 查）。
    #[must_use]
    pub fn ssh_tunnels_snapshot(&self) -> Vec<SshTunnel> {
        let conn = self.db.lock().expect("db poisoned");
        load_all_tunnels(&conn).unwrap_or_default()
    }

    /// 当前全量 RDP 转发快照（从 DB 查）。
    #[must_use]
    pub fn rdp_forwards_snapshot(&self) -> Vec<RdpForward> {
        let conn = self.db.lock().expect("db poisoned");
        load_all_rdp(&conn).unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // SSH 隧道编排
    // ------------------------------------------------------------------

    /// 启动一条 SSH 隧道：spawn `ssh` 子进程 → ~800ms 探测退出码 →
    /// 存活 `running`+pid / 失败 `failed`+stderr 摘要（不 panic）。
    ///
    /// 幂等：已有存活子进程（本进程持有 Child 或收养的 pid 仍活）→ 直接返回当前态。
    async fn start_tunnel(&self, id: &str) -> Result<SshTunnel, String> {
        let mut tunnel = {
            let conn = self.db.lock().expect("db poisoned");
            find_tunnel(&conn, id).ok_or_else(|| format!("隧道不存在: {id}"))?
        };

        // 幂等 1：本进程持有一手 Child（已退出的顺手清理后重启）
        {
            let mut children = self.children.lock().expect("children poisoned");
            if let Some(child) = children.get_mut(id) {
                match child.try_wait() {
                    Ok(None) => return Ok(tunnel), // 存活 → 不重复 spawn
                    Ok(Some(_)) => {
                        children.remove(id);
                    }
                    Err(e) => return Err(format!("探测 ssh 子进程失败: {e}")),
                }
            }
        }

        // 幂等 2：无 Child 句柄但 DB 记录 running 且 pid 仍活（重启收养）→ 不重复 spawn
        if tunnel.status == "running" && tunnel.pid.is_some_and(pid_alive) {
            return Ok(tunnel);
        }

        // spawn：stderr 落临时日志文件（探测失败时取摘要；管道满不阻塞 ssh）
        let args = build_ssh_args(&tunnel);
        let log_path = std::env::temp_dir().join(format!("os-ssh-tunnel-{id}.log"));
        let stderr = match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&log_path)
        {
            Ok(f) => Stdio::from(f),
            Err(_) => Stdio::null(),
        };
        let mut cmd = tokio::process::Command::new(ssh_bin());
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        match cmd.spawn() {
            Ok(mut child) => {
                // 探测窗口 ~800ms：ssh 连不上（密钥/网络/端口）通常立刻退出
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                match child.try_wait() {
                    Ok(Some(status)) => {
                        tunnel.status = "failed".into();
                        tunnel.pid = None;
                        tunnel.error = Some(format!(
                            "ssh 进程异常退出（{status}）{}",
                            stderr_summary(&log_path)
                        ));
                    }
                    Ok(None) => {
                        tunnel.status = "running".into();
                        tunnel.pid = child.id();
                        tunnel.error = None;
                        tunnel.last_started = Some(now_iso());
                        self.children
                            .lock()
                            .expect("children poisoned")
                            .insert(id.to_string(), child);
                    }
                    Err(e) => {
                        tunnel.status = "failed".into();
                        tunnel.pid = None;
                        tunnel.error = Some(format!("探测 ssh 子进程失败: {e}"));
                    }
                }
            }
            Err(e) => {
                tunnel.status = "failed".into();
                tunnel.pid = None;
                tunnel.error = Some(format!("ssh 命令未找到或启动失败: {e}"));
            }
        }
        {
            let conn = self.db.lock().expect("db poisoned");
            let _ = update_tunnel(&conn, &tunnel);
        }
        Ok(tunnel)
    }

    /// 停止一条 SSH 隧道：优先用内存 Child 句柄 kill（并 wait 收尸），
    /// 无句柄（重启收养）时回退 `kill <pid>`。状态置 stopped。
    async fn stop_tunnel(&self, id: &str) -> Result<SshTunnel, String> {
        let mut tunnel = {
            let conn = self.db.lock().expect("db poisoned");
            find_tunnel(&conn, id).ok_or_else(|| format!("隧道不存在: {id}"))?
        };
        // 1) 本进程 Child 句柄：精确 kill + wait（取出后再 kill，锁不跨 await）
        let owned = self.children.lock().expect("children poisoned").remove(id);
        if let Some(mut child) = owned {
            let _ = child.kill().await;
            let _ = child.wait().await;
        } else if let Some(pid) = tunnel.pid {
            // 2) 重启收养的 pid：系统 kill
            kill_pid(pid);
        }
        // 手动 stop = 用户意图：清 watchdog 失败计数，后台任务不再拉起
        self.watchdog_fails
            .lock()
            .expect("watchdog_fails poisoned")
            .remove(id);
        tunnel.status = "stopped".into();
        tunnel.pid = None;
        tunnel.error = None;
        {
            let conn = self.db.lock().expect("db poisoned");
            let _ = update_tunnel(&conn, &tunnel);
        }
        Ok(tunnel)
    }

    /// 隧道详情：实时存活探测——pid 存在且 `kill -0` 成功 = running；
    /// 已死（stale）→ stopped 并回写 DB。
    fn tunnel_detail(&self, id: &str) -> Option<SshTunnel> {
        let mut tunnel = {
            let conn = self.db.lock().expect("db poisoned");
            find_tunnel(&conn, id)?
        };
        let mut changed = false;
        // 1) 本进程 Child：try_wait 权威判定（退出的顺手回收句柄）
        {
            let mut children = self.children.lock().expect("children poisoned");
            if let Some(child) = children.get_mut(id) {
                match child.try_wait() {
                    Ok(None) => {
                        if tunnel.status != "running" {
                            tunnel.status = "running".into();
                            changed = true;
                        }
                    }
                    Ok(Some(status)) => {
                        children.remove(id);
                        tunnel.status = "stopped".into();
                        tunnel.pid = None;
                        tunnel.error = Some(format!("ssh 进程已退出（{status}）"));
                        changed = true;
                    }
                    Err(_) => {}
                }
            }
        }
        // 2) 无 Child 句柄但记录 running：kill -0 存活探测（stale → stopped）
        if tunnel.status == "running"
            && !self
                .children
                .lock()
                .expect("children poisoned")
                .contains_key(id)
        {
            let alive = tunnel.pid.map(pid_alive).unwrap_or(false);
            if !alive {
                tunnel.status = "stopped".into();
                tunnel.pid = None;
                tunnel.error = Some("ssh 进程已不存在（stale）".into());
                changed = true;
            }
        }
        if changed {
            let conn = self.db.lock().expect("db poisoned");
            let _ = update_tunnel(&conn, &tunnel);
        }
        Some(tunnel)
    }

    // ------------------------------------------------------------------
    // RDP 转发编排
    // ------------------------------------------------------------------

    /// 启动一条 RDP 转发：绑定 `0.0.0.0:<listen_port>` → spawn accept loop。
    /// 端口冲突/占用降级 `error` 状态带原因（不 panic）。幂等：已运行直接返回。
    async fn start_rdp(&self, id: &str) -> Result<RdpForward, String> {
        let mut fwd = {
            let conn = self.db.lock().expect("db poisoned");
            find_rdp(&conn, id).ok_or_else(|| format!("RDP 转发不存在: {id}"))?
        };
        if self
            .proxies
            .lock()
            .expect("proxies poisoned")
            .contains_key(id)
        {
            return Ok(fwd); // 已运行
        }
        match TcpListener::bind(("0.0.0.0", fwd.listen_port)).await {
            Ok(listener) => {
                let handle = tokio::spawn(rdp_accept_loop(
                    listener,
                    fwd.id.clone(),
                    fwd.target_host.clone(),
                    fwd.target_port,
                    Arc::clone(&self.db),
                ));
                self.proxies
                    .lock()
                    .expect("proxies poisoned")
                    .insert(id.to_string(), handle);
                fwd.status = "running".into();
                fwd.error = None;
            }
            Err(e) => {
                fwd.status = "error".into();
                fwd.error = Some(format!("监听端口 {} 绑定失败（{e}）", fwd.listen_port));
            }
        }
        {
            let conn = self.db.lock().expect("db poisoned");
            let _ = update_rdp(&conn, &fwd);
        }
        Ok(fwd)
    }

    /// 停止一条 RDP 转发：abort accept loop 并等其真正退出（listener 确定性释放），
    /// 状态置 stopped。
    async fn stop_rdp(&self, id: &str) -> Result<RdpForward, String> {
        let mut fwd = {
            let conn = self.db.lock().expect("db poisoned");
            find_rdp(&conn, id).ok_or_else(|| format!("RDP 转发不存在: {id}"))?
        };
        let owned = self.proxies.lock().expect("proxies poisoned").remove(id);
        if let Some(handle) = owned {
            handle.abort();
            // 等 accept loop 实际退出（listener 关闭），端口即时可复用
            let _ = handle.await;
        }
        fwd.status = "stopped".into();
        fwd.error = None;
        {
            let conn = self.db.lock().expect("db poisoned");
            let _ = update_rdp(&conn, &fwd);
        }
        Ok(fwd)
    }

    // ------------------------------------------------------------------
    // autostart 恢复（os-api 启动副作用）
    // ------------------------------------------------------------------

    /// 启动时恢复：先把上一进程遗留的 `running` 态做存活探测（pid 活 → 收养
    /// 保持 running；死 → stopped），再对所有 `autostart=true` 的 SSH 隧道与
    /// RDP 转发尝试 start（失败降级 failed/error，不 panic 不阻塞启动）。
    pub async fn resume_autostart(&self) {
        // 1) SSH：stale running → stopped；收集 autostart 待启列表
        let to_start: Vec<String> = {
            let conn = self.db.lock().expect("db poisoned");
            let mut pending = Vec::new();
            for mut t in load_all_tunnels(&conn).unwrap_or_default() {
                if t.status == "running" {
                    let alive = t.pid.map(pid_alive).unwrap_or(false);
                    if !alive {
                        t.status = "stopped".into();
                        t.pid = None;
                        t.error = Some("重启后 ssh 进程已不存在（stale）".into());
                        let _ = update_tunnel(&conn, &t);
                    }
                }
                if t.autostart && t.status != "running" {
                    pending.push(t.id);
                }
            }
            pending
        };
        for id in to_start {
            match self.start_tunnel(&id).await {
                Ok(t) => eprintln!("[forwarding] autostart SSH 隧道 {} → {}", t.name, t.status),
                Err(e) => eprintln!("[forwarding] autostart SSH 隧道 {id} 失败: {e}"),
            }
        }
        // 2) RDP：同策略（重启后所有代理任务都不在本进程，running 态重置后重启）
        let to_start: Vec<String> = {
            let conn = self.db.lock().expect("db poisoned");
            let mut pending = Vec::new();
            for mut f in load_all_rdp(&conn).unwrap_or_default() {
                if f.status == "running" {
                    f.status = "stopped".into();
                    let _ = update_rdp(&conn, &f);
                }
                if f.autostart && f.status != "running" {
                    pending.push(f.id);
                }
            }
            pending
        };
        for id in to_start {
            match self.start_rdp(&id).await {
                Ok(f) => eprintln!("[forwarding] autostart RDP 转发 {} → {}", f.name, f.status),
                Err(e) => eprintln!("[forwarding] autostart RDP 转发 {id} 失败: {e}"),
            }
        }
    }

    // ------------------------------------------------------------------
    // watchdog（运行期自愈：ssh 进程意外死亡 → 自动重试拉起）
    // ------------------------------------------------------------------

    /// 判定一条 `running` 态隧道的 ssh 进程是否已死：
    /// - 本进程持有 `Child` → `try_wait` 权威判定（僵尸也判死，顺手回收句柄）；
    /// - 无句柄（重启收养的 pid）→ `kill -0` 探测。
    fn tunnel_process_dead(&self, t: &SshTunnel) -> bool {
        let mut children = self.children.lock().expect("children poisoned");
        if let Some(child) = children.get_mut(&t.id) {
            match child.try_wait() {
                Ok(None) => false, // 存活
                Ok(Some(_)) => {
                    children.remove(&t.id); // 已退出，回收句柄
                    true
                }
                Err(_) => true, // 探测失败按死处理（保守：交给 start 重建）
            }
        } else {
            !t.pid.map(pid_alive).unwrap_or(false) // 无 pid 也视为死
        }
    }

    /// watchdog 单轮扫描（幂等，可独立调用——后台循环与测试共用同一入口）：
    ///
    /// 1. 只看 `watchdog=true` 的隧道；`stopped`（用户手动停）直接清计数跳过；
    /// 2. 触发条件：`running` 态但进程已死，或处于 watchdog 失败重试中
    ///    （`start` 失败会把状态写成 `failed`，内存计数器标记"我在管这条"）；
    /// 3. 重试 `start`：成功 → `restart_count`+1、`last_restart_at` 持久化、
    ///    清零失败计数；失败 → 计数 +1，连续 [`WATCHDOG_MAX_FAILURES`] 次 →
    ///    放弃：置 `failed` + error="watchdog 放弃：连续失败…"（之后本轮
    ///    扫描因非 running 且无计数不再触发，直到用户手动 start）。
    async fn watchdog_scan_once(&self) {
        let tunnels = {
            let conn = self.db.lock().expect("db poisoned");
            load_all_tunnels(&conn).unwrap_or_default()
        };
        for t in tunnels {
            if !t.watchdog {
                continue;
            }
            if t.status == "stopped" {
                // 用户手动停止：绝不拉起（顺带清掉历史失败计数）
                self.watchdog_fails
                    .lock()
                    .expect("watchdog_fails poisoned")
                    .remove(&t.id);
                continue;
            }
            let tracked = self
                .watchdog_fails
                .lock()
                .expect("watchdog_fails poisoned")
                .contains_key(&t.id);
            let dead_running = t.status == "running" && self.tunnel_process_dead(&t);
            if !dead_running && !tracked {
                continue; // 健康 / 非 watchdog 职责范围
            }
            // 尝试拉起（start_tunnel 内含 800ms 探测 + 状态回写）
            let restarted = match self.start_tunnel(&t.id).await {
                Ok(rt) => rt.status == "running",
                Err(_) => false,
            };
            if restarted {
                self.watchdog_fails
                    .lock()
                    .expect("watchdog_fails poisoned")
                    .remove(&t.id);
                {
                    let conn = self.db.lock().expect("db poisoned");
                    let _ = conn.execute(
                        "UPDATE ssh_tunnels SET restart_count = restart_count + 1,
                         last_restart_at = ?1 WHERE id = ?2",
                        params![now_iso(), t.id],
                    );
                }
                eprintln!(
                    "[forwarding] watchdog 已重启 SSH 隧道 {}（{}）",
                    t.name, t.id
                );
            } else {
                let give_up = {
                    let mut fails = self.watchdog_fails.lock().expect("watchdog_fails poisoned");
                    let n = fails.entry(t.id.clone()).or_insert(0);
                    *n += 1;
                    if *n >= WATCHDOG_MAX_FAILURES {
                        fails.remove(&t.id);
                        true
                    } else {
                        eprintln!(
                            "[forwarding] watchdog 拉起隧道 {}（{}）失败（{n}/{}）",
                            t.name, t.id, WATCHDOG_MAX_FAILURES
                        );
                        false
                    }
                };
                if give_up {
                    let conn = self.db.lock().expect("db poisoned");
                    if let Some(mut t2) = find_tunnel(&conn, &t.id) {
                        t2.status = "failed".into();
                        t2.pid = None;
                        t2.error = Some(format!(
                            "watchdog 放弃：连续失败 {WATCHDOG_MAX_FAILURES} 次"
                        ));
                        let _ = update_tunnel(&conn, &t2);
                    }
                    eprintln!(
                        "[forwarding] watchdog 放弃隧道 {}（{}）：连续失败 {WATCHDOG_MAX_FAILURES} 次",
                        t.name, t.id
                    );
                }
            }
        }
    }

    /// 后台启动 watchdog 循环（每 [`watchdog_secs`] 秒一轮
    /// [`Self::watchdog_scan_once`]）。main.rs 装配经
    /// [`Self::spawn_autostart_resume`] 一并拉起，不阻塞启动流程。
    pub fn spawn_watchdog(&self) {
        let tmp = ForwardingRouteHandler {
            db: Arc::clone(&self.db),
            children: Arc::clone(&self.children),
            proxies: Arc::clone(&self.proxies),
            watchdog_fails: Arc::clone(&self.watchdog_fails),
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(watchdog_secs())).await;
                tmp.watchdog_scan_once().await;
            }
        });
    }

    /// 后台任务方式恢复 autostart（main.rs 装配处调用，不阻塞启动流程；
    /// 参考 monitor handler 的 `spawn_alert_engine` 先例）。
    ///
    /// task 持三份 `Arc` clone 构造临时 handler 与注册进网关的本体共享同一状态。
    /// 2026-08-20 起一并拉起 watchdog 看护循环（运行期 ssh 死亡自愈）。
    pub fn spawn_autostart_resume(&self) {
        let tmp = ForwardingRouteHandler {
            db: Arc::clone(&self.db),
            children: Arc::clone(&self.children),
            proxies: Arc::clone(&self.proxies),
            watchdog_fails: Arc::clone(&self.watchdog_fails),
        };
        tokio::spawn(async move {
            tmp.resume_autostart().await;
        });
        self.spawn_watchdog();
    }
}

impl Default for ForwardingRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for ForwardingRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— SSH 隧道 ——
            spec(HttpMethod::Get, PATH_SSH_LIST, false, vec![]),
            spec(HttpMethod::Post, PATH_SSH_LIST, true, vec!["admin".into()]),
            spec(HttpMethod::Get, PATH_SSH_DETAIL, false, vec![]),
            spec(
                HttpMethod::Delete,
                PATH_SSH_DETAIL,
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Post, PATH_SSH_START, true, vec!["admin".into()]),
            spec(HttpMethod::Post, PATH_SSH_STOP, true, vec!["admin".into()]),
            // —— RDP 转发 ——
            spec(HttpMethod::Get, PATH_RDP_LIST, false, vec![]),
            spec(HttpMethod::Post, PATH_RDP_LIST, true, vec!["admin".into()]),
            spec(
                HttpMethod::Delete,
                PATH_RDP_DETAIL,
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Post, PATH_RDP_START, true, vec!["admin".into()]),
            spec(HttpMethod::Post, PATH_RDP_STOP, true, vec!["admin".into()]),
            spec(HttpMethod::Get, PATH_RDP_FILE, false, vec![]),
            // —— 聚合 ——
            spec(HttpMethod::Get, PATH_STATS, false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = req.path.split('?').nth(1).unwrap_or("");
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/forwarding/ssh —— 隧道列表
            (HttpMethod::Get, ["api", "v1", "forwarding", "ssh"]) => {
                let list = self.ssh_tunnels_snapshot();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/forwarding/ssh —— 创建隧道（密钥认证红线：拒 password 字段）
            (HttpMethod::Post, ["api", "v1", "forwarding", "ssh"]) => {
                if req.body.get("password").is_some() {
                    return Ok(error_response(
                        400,
                        "SSH 隧道仅支持密钥认证（private_key_path），不接受 password 字段",
                    ));
                }
                let body: CreateSshReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建隧道请求体失败: {e}"))
                })?;
                if let Some(msg) = validate_ssh_body(&body) {
                    return Ok(error_response(400, &msg));
                }
                let mode = body
                    .mode
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| "local".to_string());
                let tunnel = SshTunnel {
                    id: new_uuid(),
                    name: body.name.trim().to_string(),
                    ssh_host: body.ssh_host.trim().to_string(),
                    ssh_port: body.ssh_port.unwrap_or(22) as u16,
                    ssh_user: body.ssh_user.trim().to_string(),
                    private_key_path: body.private_key_path.filter(|p| !p.trim().is_empty()),
                    mode,
                    local_bind: body.local_bind.trim().to_string(),
                    remote_host: body.remote_host.filter(|h| !h.trim().is_empty()),
                    remote_port: body.remote_port.map(|p| p as u16),
                    autostart: body.autostart,
                    watchdog: body.watchdog,
                    status: "stopped".to_string(),
                    pid: None,
                    error: None,
                    created_at: now_iso(),
                    last_started: None,
                    restart_count: 0,
                    last_restart_at: None,
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_tunnel(&conn, &tunnel)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&tunnel)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/forwarding/ssh/:id —— 详情（实时存活探测）
            (HttpMethod::Get, ["api", "v1", "forwarding", "ssh", id]) => {
                match self.tunnel_detail(id) {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("隧道不存在: {id}"))),
                }
            }

            // —— DELETE /api/v1/forwarding/ssh/:id —— 删（运行中先停）
            (HttpMethod::Delete, ["api", "v1", "forwarding", "ssh", id]) => {
                let existed = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_tunnel(&conn, id).is_some()
                };
                if !existed {
                    return Ok(error_response(404, &format!("隧道不存在: {id}")));
                }
                let stopped = self
                    .stop_tunnel(id)
                    .await
                    .map_err(ApiGatewayError::Internal)?;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    delete_tunnel(&conn, id)?;
                }
                Ok(ok_json(serde_json::json!({
                    "deleted": id,
                    "last_status": stopped.status,
                })))
            }

            // —— POST /api/v1/forwarding/ssh/:id/start —— 启动（spawn ssh）
            (HttpMethod::Post, ["api", "v1", "forwarding", "ssh", id, "start"]) => {
                match self.start_tunnel(id).await {
                    Ok(t) => Ok(ok_json(to_value(&t)?)),
                    Err(e) if e.starts_with("隧道不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(500, &e)),
                }
            }

            // —— POST /api/v1/forwarding/ssh/:id/stop —— 停止（kill 子进程）
            (HttpMethod::Post, ["api", "v1", "forwarding", "ssh", id, "stop"]) => {
                match self.stop_tunnel(id).await {
                    Ok(t) => Ok(ok_json(to_value(&t)?)),
                    Err(e) if e.starts_with("隧道不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(500, &e)),
                }
            }

            // —— GET /api/v1/forwarding/rdp —— 转发列表
            (HttpMethod::Get, ["api", "v1", "forwarding", "rdp"]) => {
                let list = self.rdp_forwards_snapshot();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/forwarding/rdp —— 创建转发
            (HttpMethod::Post, ["api", "v1", "forwarding", "rdp"]) => {
                let body: CreateRdpReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建 RDP 转发请求体失败: {e}"))
                })?;
                if let Some(msg) = validate_rdp_body(&body) {
                    return Ok(error_response(400, &msg));
                }
                let fwd = RdpForward {
                    id: new_uuid(),
                    name: body.name.trim().to_string(),
                    target_host: body.target_host.trim().to_string(),
                    target_port: body.target_port.unwrap_or(3389) as u16,
                    listen_port: body.listen_port as u16,
                    autostart: body.autostart,
                    status: "stopped".to_string(),
                    connections: 0,
                    error: None,
                    created_at: now_iso(),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_rdp(&conn, &fwd)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&fwd)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/forwarding/rdp/:id —— 删（运行中先停）
            (HttpMethod::Delete, ["api", "v1", "forwarding", "rdp", id]) => {
                let existed = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_rdp(&conn, id).is_some()
                };
                if !existed {
                    return Ok(error_response(404, &format!("RDP 转发不存在: {id}")));
                }
                let stopped = self.stop_rdp(id).await.map_err(ApiGatewayError::Internal)?;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    delete_rdp(&conn, id)?;
                }
                Ok(ok_json(serde_json::json!({
                    "deleted": id,
                    "last_status": stopped.status,
                })))
            }

            // —— POST /api/v1/forwarding/rdp/:id/start —— 启动 TCP 代理
            (HttpMethod::Post, ["api", "v1", "forwarding", "rdp", id, "start"]) => {
                match self.start_rdp(id).await {
                    Ok(f) => Ok(ok_json(to_value(&f)?)),
                    Err(e) if e.starts_with("RDP 转发不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(500, &e)),
                }
            }

            // —— POST /api/v1/forwarding/rdp/:id/stop —— 停止代理
            (HttpMethod::Post, ["api", "v1", "forwarding", "rdp", id, "stop"]) => {
                match self.stop_rdp(id).await {
                    Ok(f) => Ok(ok_json(to_value(&f)?)),
                    Err(e) if e.starts_with("RDP 转发不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(500, &e)),
                }
            }

            // —— GET /api/v1/forwarding/rdp/:id/rdp-file?username= —— 下载 .rdp 配置
            (HttpMethod::Get, ["api", "v1", "forwarding", "rdp", id, "rdp-file"]) => {
                let fwd = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_rdp(&conn, id)
                };
                let Some(fwd) = fwd else {
                    return Ok(error_response(404, &format!("RDP 转发不存在: {id}")));
                };
                let username = parse_query_str(query, "username")
                    .filter(|u| !u.trim().is_empty())
                    .map(|u| url_decode(&u));
                let host = resolve_local_host(&req.headers);
                let content = build_rdp_file(&host, fwd.listen_port, username.as_deref());
                let filename = sanitize_filename(&fwd.name);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::Value::String(content),
                    headers: serde_json::json!({
                        "content-type": "application/rdp",
                        "content-disposition":
                            format!("attachment; filename=\"{filename}.rdp\""),
                    }),
                })
            }

            // —— GET /api/v1/forwarding/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "forwarding", "stats"]) => {
                let stats = {
                    let conn = self.db.lock().expect("db poisoned");
                    let tunnels = load_all_tunnels(&conn).unwrap_or_default();
                    let rdp = load_all_rdp(&conn).unwrap_or_default();
                    ForwardingStats {
                        ssh_tunnels_total: tunnels.len(),
                        ssh_tunnels_running: tunnels
                            .iter()
                            .filter(|t| t.status == "running")
                            .count(),
                        rdp_forwards_total: rdp.len(),
                        rdp_forwards_running: rdp.iter().filter(|f| f.status == "running").count(),
                        rdp_total_connections: rdp.iter().map(|f| f.connections).sum(),
                    }
                };
                Ok(ok_json(to_value(&stats)?))
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "forwarding: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// `GET/POST /api/v1/forwarding/ssh`
const PATH_SSH_LIST: &str = "/api/v1/forwarding/ssh";
/// `GET/DELETE /api/v1/forwarding/ssh/:id`
const PATH_SSH_DETAIL: &str = "/api/v1/forwarding/ssh/:id";
/// `POST /api/v1/forwarding/ssh/:id/start`
const PATH_SSH_START: &str = "/api/v1/forwarding/ssh/:id/start";
/// `POST /api/v1/forwarding/ssh/:id/stop`
const PATH_SSH_STOP: &str = "/api/v1/forwarding/ssh/:id/stop";
/// `GET/POST /api/v1/forwarding/rdp`
const PATH_RDP_LIST: &str = "/api/v1/forwarding/rdp";
/// `DELETE /api/v1/forwarding/rdp/:id`
const PATH_RDP_DETAIL: &str = "/api/v1/forwarding/rdp/:id";
/// `POST /api/v1/forwarding/rdp/:id/start`
const PATH_RDP_START: &str = "/api/v1/forwarding/rdp/:id/start";
/// `POST /api/v1/forwarding/rdp/:id/stop`
const PATH_RDP_STOP: &str = "/api/v1/forwarding/rdp/:id/stop";
/// `GET /api/v1/forwarding/rdp/:id/rdp-file`
const PATH_RDP_FILE: &str = "/api/v1/forwarding/rdp/:id/rdp-file";
/// `GET /api/v1/forwarding/stats`
const PATH_STATS: &str = "/api/v1/forwarding/stats";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "forwarding";

/// 隧道模式合法取值。
const VALID_MODES: [&str; 3] = ["local", "remote", "dynamic"];

/// watchdog 连续拉起失败放弃阈值（达到即标 failed + "watchdog 放弃"）。
const WATCHDOG_MAX_FAILURES: u32 = 5;

/// watchdog 扫描间隔（秒）：环境变量 `NEXOS_FORWARDING_WATCHDOG_SECS` 覆写
/// （解析失败或 <1 一律回退默认），默认 30。每个扫描 tick 重新读取——
/// 改 env 后无需重启（下一轮生效）。
fn watchdog_secs() -> u64 {
    std::env::var("NEXOS_FORWARDING_WATCHDOG_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s >= 1)
        .unwrap_or(30)
}

/// 创建隧道的请求体（端口用 u64 承载以给出干净的 400，而非 serde 500）。
#[derive(Debug, Deserialize)]
struct CreateSshReq {
    name: String,
    ssh_host: String,
    ssh_user: String,
    #[serde(default)]
    ssh_port: Option<u64>,
    #[serde(default)]
    private_key_path: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    local_bind: String,
    #[serde(default)]
    remote_host: Option<String>,
    #[serde(default)]
    remote_port: Option<u64>,
    #[serde(default)]
    autostart: bool,
    /// 运行期 watchdog 看护（ssh 进程死→自动重试拉起，连续失败 5 次放弃）。
    /// 缺省 false（存量请求体兼容）。
    #[serde(default)]
    watchdog: bool,
}

/// 创建 RDP 转发的请求体。
#[derive(Debug, Deserialize)]
struct CreateRdpReq {
    name: String,
    target_host: String,
    #[serde(default)]
    target_port: Option<u64>,
    listen_port: u64,
    #[serde(default)]
    autostart: bool,
}

/// 校验创建隧道请求体，返回 Some(错误消息) 表示 400。
fn validate_ssh_body(body: &CreateSshReq) -> Option<String> {
    if body.name.trim().is_empty() {
        return Some("name 不可为空".into());
    }
    if body.ssh_host.trim().is_empty() {
        return Some("ssh_host 不可为空".into());
    }
    if body.ssh_user.trim().is_empty() {
        return Some("ssh_user 不可为空".into());
    }
    let port = body.ssh_port.unwrap_or(22);
    if !valid_port(port) {
        return Some(format!("ssh_port 须为 1..=65535（当前 {port}）"));
    }
    let mode = body
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("local");
    if !VALID_MODES.contains(&mode) {
        return Some(format!("mode 取值须为 {VALID_MODES:?}（当前 {mode}）"));
    }
    let bind = body.local_bind.trim();
    if bind.is_empty() {
        return Some("local_bind 不可为空".into());
    }
    if !valid_bind(bind) {
        return Some(format!("local_bind 须为 host:port 形式（当前 {bind}）"));
    }
    let remote_host = body
        .remote_host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let remote_port = body.remote_port;
    match mode {
        "dynamic" => {
            if remote_host.is_some() || remote_port.is_some() {
                return Some("dynamic（-D SOCKS）模式不支持 remote_host/remote_port".into());
            }
        }
        // local / remote：目标必填
        _ => {
            if remote_host.is_none() {
                return Some(format!("{mode} 模式必须提供 remote_host"));
            }
            let Some(rp) = remote_port else {
                return Some(format!("{mode} 模式必须提供 remote_port"));
            };
            if !valid_port(rp) {
                return Some(format!("remote_port 须为 1..=65535（当前 {rp}）"));
            }
        }
    }
    None
}

/// 校验创建 RDP 转发请求体，返回 Some(错误消息) 表示 400。
fn validate_rdp_body(body: &CreateRdpReq) -> Option<String> {
    if body.name.trim().is_empty() {
        return Some("name 不可为空".into());
    }
    if body.target_host.trim().is_empty() {
        return Some("target_host 不可为空".into());
    }
    if let Some(tp) = body.target_port {
        if !valid_port(tp) {
            return Some(format!("target_port 须为 1..=65535（当前 {tp}）"));
        }
    }
    if !valid_port(body.listen_port) {
        return Some(format!(
            "listen_port 须为 1..=65535（当前 {}）",
            body.listen_port
        ));
    }
    None
}

/// 端口合法（1..=65535；0 非法）。
fn valid_port(p: u64) -> bool {
    (1..=u64::from(u16::MAX)).contains(&p)
}

/// `host:port` 形式校验（不做 DNS 解析，纯格式）。
fn valid_bind(bind: &str) -> bool {
    match bind.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0),
        None => false,
    }
}

/// ssh 二进制路径：环境变量 `NEXOS_SSH_BIN` 覆写（测试注入 /bin/false 或脚本），
/// 默认系统 `ssh`。
fn ssh_bin() -> String {
    std::env::var("NEXOS_SSH_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "ssh".to_string())
}

/// 构造 ssh 隧道命令参数（纯函数）：
/// `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=30
///  -o StrictHostKeyChecking=accept-new -i <key> -p <port> [-L|-R|-D ...] user@host`
///
/// 密钥认证红线：`BatchMode=yes` 禁密码交互——ssh 无密钥即失败退出，绝不挂起等密码。
#[must_use]
pub fn build_ssh_args(t: &SshTunnel) -> Vec<String> {
    let key = t
        .private_key_path
        .clone()
        .unwrap_or_else(|| "~/.ssh/id_ed25519".to_string());
    let mut args = vec![
        "-N".to_string(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-i".into(),
        key,
        "-p".into(),
        t.ssh_port.to_string(),
    ];
    match t.mode.as_str() {
        // 本地转发：本地 local_bind → 经 SSH 服务器 → remote_host:remote_port
        "local" => {
            args.push("-L".into());
            args.push(format!(
                "{}:{}:{}",
                t.local_bind,
                t.remote_host.clone().unwrap_or_default(),
                t.remote_port.unwrap_or(0)
            ));
        }
        // 远程转发：SSH 服务器上的 local_bind（当作远端绑定）→ 本机可达的 remote_host:port
        "remote" => {
            args.push("-R".into());
            args.push(format!(
                "{}:{}:{}",
                t.local_bind,
                t.remote_host.clone().unwrap_or_default(),
                t.remote_port.unwrap_or(0)
            ));
        }
        // 动态 SOCKS 代理：本地 local_bind 起 SOCKS5
        _ => {
            args.push("-D".into());
            args.push(t.local_bind.clone());
        }
    }
    args.push(format!("{}@{}", t.ssh_user, t.ssh_host));
    args
}

/// 读取 stderr 日志摘要（末尾 ~300 字符，去掉首尾空白；读不到返回空串）。
fn stderr_summary(log_path: &std::path::Path) -> String {
    let tail = std::fs::read_to_string(log_path)
        .unwrap_or_default()
        .trim()
        .chars()
        .rev()
        .take(300)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    }
}

/// pid 存活探测（`kill -0`）：成功 = 进程存在。无 libc 依赖，经系统 kill 命令。
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// 系统 kill 一个 pid（SIGTERM；重启收养的无 Child 句柄进程用）。
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 解析 .rdp 文件里的本机地址：优先 Host 头（去掉端口部分），回退 cached_hostname。
fn resolve_local_host(headers: &serde_json::Value) -> String {
    if let Some(host) = headers.get("host").and_then(|v| v.as_str()) {
        let bare = host.rsplit_once(':').map_or(host, |(h, _)| h);
        if !bare.trim().is_empty() {
            return bare.trim().to_string();
        }
    }
    cached_hostname()
}

/// 本机 hostname（缓存；可用 `NEXOS_FORWARDING_HOST` / `OS_FORWARDING_HOST` 覆写，
/// 默认 `hostname` 命令，再回退 `localhost`）。模式同 os-nexhub code_repo。
fn cached_hostname() -> String {
    use std::sync::OnceLock;
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::env::var("NEXOS_FORWARDING_HOST")
            .or_else(|_| std::env::var("OS_FORWARDING_HOST"))
            .ok()
            .filter(|s| !s.trim().is_empty())
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

/// 生成 .rdp 客户端配置文件内容（纯函数）。
///
/// `full address` = `<host>:<port>`（RDP 代理入口）；`username` 为空则省略该行；
/// 其余为常见合理默认（全屏 / 32bpp / 允许断线重连 / 剪贴板重定向）。
#[must_use]
pub fn build_rdp_file(host: &str, port: u16, username: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("screen mode id:i:2\n"); // 全屏
    s.push_str("use multimon:i:0\n");
    s.push_str("desktopwidth:i:1920\n");
    s.push_str("desktopheight:i:1080\n");
    s.push_str("session bpp:i:32\n");
    s.push_str("winposstr:s:0,3,0,0,800,600\n");
    s.push_str("allow font smoothing:i:1\n");
    s.push_str("allow desktop composition:i:1\n");
    s.push_str("disable wallpaper:i:0\n");
    if let Some(u) = username.filter(|u| !u.trim().is_empty()) {
        s.push_str(&format!("username:s:{u}\n"));
    }
    s.push_str(&format!("full address:s:{host}:{port}\n"));
    s.push_str("autoreconnection enabled:i:1\n");
    s.push_str("authentication level:i:2\n");
    s.push_str("redirectprinters:i:0\n");
    s.push_str("redirectclipboard:i:1\n");
    s.push_str("audiomode:i:0\n");
    s.push_str("# smart sizing（窗口自适应缩放）: 删除下一行行首 # 启用\n");
    s.push_str("# smart sizing:s:1\n");
    s
}

/// Content-Disposition 文件名净化（非 Unicode 字母数字/-/_/. 全部替换为 '-'，
/// 中文等非 ASCII 字母保留——NexOS 面向中文用户，RDP 客户端普遍接受 UTF-8 文件名）。
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "forward".to_string()
    } else {
        cleaned
    }
}

/// RDP 代理 accept loop：每连接 spawn 一个 `copy_bidirectional` 双向拷贝任务，
/// accept 计数（持久化累计连接数）。listener 释放（stop abort）后退出循环。
async fn rdp_accept_loop(
    listener: TcpListener,
    id: String,
    target_host: String,
    target_port: u16,
    db: Arc<Mutex<Connection>>,
) {
    loop {
        match listener.accept().await {
            Ok((mut inbound, _peer)) => {
                {
                    let conn = db.lock().expect("db poisoned");
                    let _ = bump_rdp_connections(&conn, &id);
                }
                let host = target_host.clone();
                let conn_id = id.clone();
                tokio::spawn(async move {
                    match TcpStream::connect((host.as_str(), target_port)).await {
                        Ok(mut upstream) => {
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await;
                        }
                        Err(e) => eprintln!(
                            "[forwarding] rdp {conn_id} 上游 {host}:{target_port} 连接失败（{e}）"
                        ),
                    }
                });
            }
            Err(e) => {
                // listener 被回收/异常关闭 → 退出循环（正常 stop 即此路径）
                eprintln!("[forwarding] rdp {id} accept 退出（{e}）");
                break;
            }
        }
    }
}

fn default_ssh_port() -> u16 {
    22
}
fn default_rdp_port() -> u16 {
    3389
}
fn default_mode_local() -> String {
    "local".to_string()
}
fn default_status_stopped() -> String {
    "stopped".to_string()
}

/// 构造一条 [`RouteSpec`]（component 固定 `forwarding`；读免认证，写要求 admin）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles,
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
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

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从 query string 解析字符串参数。
fn parse_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(|s| s.to_string());
        }
    }
    None
}

/// 极简 percent-decoding（`%XX` → 字节，`+` → 空格；坏序列原样保留）。
fn url_decode(s: &str) -> String {
    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 当前本地时间（RFC3339 / ISO8601 带时区）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 生成一个新的 UUID v4 字符串。
fn new_uuid() -> String {
    os_core::Uuid::new_v4().to_string()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 `/tank/os-data/forwarding.db`，再 `/var/lib/os/forwarding.db`，
/// 最后 `./forwarding.db`（保底）。
fn default_db_path() -> String {
    for p in &["/tank/os-data/forwarding.db", "/var/lib/os/forwarding.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./forwarding.db".to_string()
}

/// 打开 SQLite 文件，建表（不 seed——真实工具配置，见模块头说明）。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ 存量库迁移（ALTER TABLE 补列，已存在则忽略）。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ssh_tunnels (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            ssh_host TEXT NOT NULL,
            ssh_port INTEGER NOT NULL DEFAULT 22,
            ssh_user TEXT NOT NULL,
            private_key_path TEXT,
            mode TEXT NOT NULL DEFAULT 'local',
            local_bind TEXT NOT NULL,
            remote_host TEXT,
            remote_port INTEGER,
            autostart INTEGER NOT NULL DEFAULT 0,
            watchdog INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'stopped',
            pid INTEGER,
            error TEXT,
            created_at TEXT,
            last_started TEXT,
            restart_count INTEGER NOT NULL DEFAULT 0,
            last_restart_at TEXT
        );
        CREATE TABLE IF NOT EXISTS rdp_forwards (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            target_host TEXT NOT NULL,
            target_port INTEGER NOT NULL DEFAULT 3389,
            listen_port INTEGER NOT NULL,
            autostart INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'stopped',
            connections INTEGER NOT NULL DEFAULT 0,
            error TEXT,
            created_at TEXT
        );",
    )?;
    // 迁移：2026-08-20 之前的 ssh_tunnels 表缺 watchdog/restart_count/last_restart_at
    // 三列（CREATE IF NOT EXISTS 不会给已存在的表补列）。列已存在时 ALTER 报
    // "duplicate column" —— 忽略即可（幂等）。
    for ddl in [
        "ALTER TABLE ssh_tunnels ADD COLUMN watchdog INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE ssh_tunnels ADD COLUMN restart_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE ssh_tunnels ADD COLUMN last_restart_at TEXT",
    ] {
        let _ = conn.execute(ddl, []);
    }
    Ok(())
}

// ---- ssh_tunnels CRUD ----

fn insert_tunnel(conn: &Connection, t: &SshTunnel) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ssh_tunnels
         (id,name,ssh_host,ssh_port,ssh_user,private_key_path,mode,local_bind,
          remote_host,remote_port,autostart,watchdog,status,pid,error,created_at,
          last_started,restart_count,last_restart_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            t.id,
            t.name,
            t.ssh_host,
            i64::from(t.ssh_port),
            t.ssh_user,
            t.private_key_path.as_deref(),
            t.mode,
            t.local_bind,
            t.remote_host.as_deref(),
            t.remote_port.map(i64::from),
            t.autostart as i64,
            t.watchdog as i64,
            t.status,
            t.pid.map(i64::from),
            t.error.as_deref(),
            t.created_at,
            t.last_started.as_deref(),
            i64::from(t.restart_count),
            t.last_restart_at.as_deref(),
        ],
    )?;
    Ok(())
}

fn update_tunnel(conn: &Connection, t: &SshTunnel) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE ssh_tunnels SET
            name=?, ssh_host=?, ssh_port=?, ssh_user=?, private_key_path=?, mode=?,
            local_bind=?, remote_host=?, remote_port=?, autostart=?, watchdog=?,
            status=?, pid=?, error=?, created_at=?, last_started=?,
            restart_count=?, last_restart_at=?
         WHERE id=?",
        params![
            t.name,
            t.ssh_host,
            i64::from(t.ssh_port),
            t.ssh_user,
            t.private_key_path.as_deref(),
            t.mode,
            t.local_bind,
            t.remote_host.as_deref(),
            t.remote_port.map(i64::from),
            t.autostart as i64,
            t.watchdog as i64,
            t.status,
            t.pid.map(i64::from),
            t.error.as_deref(),
            t.created_at,
            t.last_started.as_deref(),
            i64::from(t.restart_count),
            t.last_restart_at.as_deref(),
            t.id,
        ],
    )?;
    Ok(())
}

fn delete_tunnel(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM ssh_tunnels WHERE id=?", params![id])
}

/// ssh_tunnels 全列 SELECT（19 列，顺序与 [`tunnel_from_row`] 的行索引对齐）。
const TUNNEL_COLS: &str = "id,name,ssh_host,ssh_port,ssh_user,private_key_path,mode,local_bind,
                remote_host,remote_port,autostart,watchdog,status,pid,error,created_at,
                last_started,restart_count,last_restart_at";

fn find_tunnel(conn: &Connection, id: &str) -> Option<SshTunnel> {
    conn.query_row(
        &format!("SELECT {TUNNEL_COLS} FROM ssh_tunnels WHERE id=?"),
        params![id],
        tunnel_from_row,
    )
    .optional()
    .unwrap_or(None)
}

fn load_all_tunnels(conn: &Connection) -> rusqlite::Result<Vec<SshTunnel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TUNNEL_COLS} FROM ssh_tunnels ORDER BY created_at"
    ))?;
    let iter = stmt.query_map([], tunnel_from_row)?;
    let mut out = Vec::new();
    for t in iter {
        out.push(t?);
    }
    Ok(out)
}

fn tunnel_from_row(row: &rusqlite::Row) -> rusqlite::Result<SshTunnel> {
    Ok(SshTunnel {
        id: row.get(0)?,
        name: row.get(1)?,
        ssh_host: row.get(2)?,
        ssh_port: row.get::<_, i64>(3)?.clamp(0, i64::from(u16::MAX)) as u16,
        ssh_user: row.get(4)?,
        private_key_path: row.get(5)?,
        mode: row
            .get::<_, Option<String>>(6)?
            .unwrap_or_else(|| "local".into()),
        local_bind: row.get(7)?,
        remote_host: row.get(8)?,
        remote_port: row.get::<_, Option<i64>>(9)?.map(|p| p as u16),
        autostart: row.get::<_, i64>(10)? != 0,
        watchdog: row.get::<_, i64>(11)? != 0,
        status: row
            .get::<_, Option<String>>(12)?
            .unwrap_or_else(|| "stopped".into()),
        pid: row.get::<_, Option<i64>>(13)?.map(|p| p as u32),
        error: row.get(14)?,
        created_at: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
        last_started: row.get(16)?,
        restart_count: row.get::<_, Option<i64>>(17)?.unwrap_or(0).max(0) as u32,
        last_restart_at: row.get(18)?,
    })
}

// ---- rdp_forwards CRUD ----

fn insert_rdp(conn: &Connection, f: &RdpForward) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO rdp_forwards
         (id,name,target_host,target_port,listen_port,autostart,status,connections,error,created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            f.id,
            f.name,
            f.target_host,
            i64::from(f.target_port),
            i64::from(f.listen_port),
            f.autostart as i64,
            f.status,
            f.connections as i64,
            f.error.as_deref(),
            f.created_at,
        ],
    )?;
    Ok(())
}

fn update_rdp(conn: &Connection, f: &RdpForward) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE rdp_forwards SET
            name=?, target_host=?, target_port=?, listen_port=?, autostart=?,
            status=?, connections=?, error=?, created_at=?
         WHERE id=?",
        params![
            f.name,
            f.target_host,
            i64::from(f.target_port),
            i64::from(f.listen_port),
            f.autostart as i64,
            f.status,
            f.connections as i64,
            f.error.as_deref(),
            f.created_at,
            f.id,
        ],
    )?;
    Ok(())
}

fn delete_rdp(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM rdp_forwards WHERE id=?", params![id])
}

fn find_rdp(conn: &Connection, id: &str) -> Option<RdpForward> {
    conn.query_row(
        "SELECT id,name,target_host,target_port,listen_port,autostart,status,connections,error,created_at
         FROM rdp_forwards WHERE id=?",
        params![id],
        rdp_from_row,
    )
    .optional()
    .unwrap_or(None)
}

fn load_all_rdp(conn: &Connection) -> rusqlite::Result<Vec<RdpForward>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,target_host,target_port,listen_port,autostart,status,connections,error,created_at
         FROM rdp_forwards ORDER BY created_at",
    )?;
    let iter = stmt.query_map([], rdp_from_row)?;
    let mut out = Vec::new();
    for f in iter {
        out.push(f?);
    }
    Ok(out)
}

fn rdp_from_row(row: &rusqlite::Row) -> rusqlite::Result<RdpForward> {
    Ok(RdpForward {
        id: row.get(0)?,
        name: row.get(1)?,
        target_host: row.get(2)?,
        target_port: row.get::<_, i64>(3)?.clamp(0, i64::from(u16::MAX)) as u16,
        listen_port: row.get::<_, i64>(4)?.clamp(0, i64::from(u16::MAX)) as u16,
        autostart: row.get::<_, i64>(5)? != 0,
        status: row
            .get::<_, Option<String>>(6)?
            .unwrap_or_else(|| "stopped".into()),
        connections: row.get::<_, i64>(7)?.max(0) as u64,
        error: row.get(8)?,
        created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
    })
}

/// 累计连接数 +1（accept loop 内短锁执行）。
fn bump_rdp_connections(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE rdp_forwards SET connections = connections + 1 WHERE id=?",
        params![id],
    )?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 单元测（不起真实外部网络：ssh 经 NEXOS_SSH_BIN 注入 /bin/false 或脚本；
// RDP 代理仅绑定 127.0.0.1 本机回环）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// NEXOS_SSH_BIN 触碰互斥：并行测试下 env 是进程级全局，凡 set/remove 该
    /// 变量的测试必须持此锁串行（api_gateway.rs 的 env 测试同款思路）。
    /// 内层用 tokio Mutex：guard 允许跨 await 持有（clippy await_holding_lock 友好）；
    /// 外层 OnceLock 同 os-nexhub code_repo::cached_hostname 惯例（非 const 构造）。
    static SSH_BIN_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn empty_handler() -> ForwardingRouteHandler {
        ForwardingRouteHandler::with_empty()
    }

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn get_req_with_host(path: &str, host: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({ "host": host }),
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

    fn create_tunnel_req() -> serde_json::Value {
        serde_json::json!({
            "name": "内网穿透",
            "ssh_host": "192.168.1.100",
            "ssh_user": "oem",
            "mode": "local",
            "local_bind": "127.0.0.1:8080",
            "remote_host": "10.0.0.5",
            "remote_port": 80,
        })
    }

    /// 找一个本测试进程内**互不重复**的空闲 TCP 端口：bind 试探成功即返回。
    ///
    /// 不用 bind(:0) 取端口的方式——并行测试下两次 :0 会拿到同一个刚释放的
    /// 端口造成互相冲突；这里用进程 id 派生基数 + 原子计数器逐个递增探测，
    /// 保证同进程内每次调用得到不同端口。
    async fn unique_free_port() -> u16 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static PORT_COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);
            let candidate = 33000 + ((u64::from(std::process::id()) * 997 + n * 37) % 20000) as u16;
            // 用 0.0.0.0 试探（与被测 handler 的绑定面一致，比 127.0.0.1 更严）
            if let Ok(l) = tokio::net::TcpListener::bind(("0.0.0.0", candidate)).await {
                drop(l);
                return candidate;
            }
        }
    }

    // 1. 路由表 + 归属 + 鉴权矩阵（13 条：GET 免认证，写操作 admin）
    #[tokio::test]
    async fn routes_declares_all_forwarding_endpoints() {
        let h = empty_handler();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 13, "应声明 13 条路由");
        assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
        for r in &routes {
            match r.method {
                HttpMethod::Get => {
                    assert!(!r.requires_auth, "GET 应公开: {r:?}");
                }
                _ => {
                    assert!(r.requires_auth, "写操作需 auth: {r:?}");
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                }
            }
        }
    }

    // 2. SSH 隧道 CRUD roundtrip（创建默认值 / 列表 / 详情 / 删除）
    #[tokio::test]
    async fn ssh_crud_roundtrip() {
        let h = empty_handler();
        // 创建：端口默认 22、私钥默认 None、状态 stopped
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["name"], "内网穿透");
        assert_eq!(resp.body["ssh_port"], 22);
        assert_eq!(resp.body["status"], "stopped");
        assert_eq!(resp.body["pid"], serde_json::Value::Null);
        assert!(resp.body["private_key_path"].is_null());
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表
        let list = h.handle(get_req(PATH_SSH_LIST)).await.unwrap();
        assert_eq!(list.body.as_array().unwrap().len(), 1);
        // 详情
        let detail = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(detail.status, 200);
        assert_eq!(detail.body["id"], id);
        // 删除 → 详情 404
        let del = h
            .handle(del_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(del.status, 200);
        assert_eq!(del.body["deleted"], id);
        let gone = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(gone.status, 404);
        // snapshot（DB 真实写入）也清空
        assert!(h.ssh_tunnels_snapshot().is_empty());
    }

    // 3. 创建校验：dynamic 带 remote_host 400 / 端口非法 400 / 模式非法 400 /
    //    local 缺 remote_* 400 / password 字段红线 400
    #[tokio::test]
    async fn ssh_create_validation_matrix() {
        let h = empty_handler();
        // dynamic 带 remote_host → 400
        let mut body = serde_json::json!({
            "name": "socks", "ssh_host": "h", "ssh_user": "u",
            "mode": "dynamic", "local_bind": "127.0.0.1:1080",
            "remote_host": "10.0.0.1",
        });
        let resp = h
            .handle(post_req(PATH_SSH_LIST, body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "dynamic + remote_host 应 400");
        // dynamic 带 remote_port → 400
        body["remote_host"] = serde_json::Value::Null;
        body["remote_port"] = serde_json::json!(80);
        let resp = h
            .handle(post_req(PATH_SSH_LIST, body.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "dynamic + remote_port 应 400");
        // 端口非法：ssh_port=0 / remote_port=0 / ssh_port=65536
        let mut b2 = create_tunnel_req();
        b2["ssh_port"] = serde_json::json!(0);
        let resp = h.handle(post_req(PATH_SSH_LIST, b2.clone())).await.unwrap();
        assert_eq!(resp.status, 400, "ssh_port=0 应 400");
        b2["ssh_port"] = serde_json::json!(65536);
        let resp = h.handle(post_req(PATH_SSH_LIST, b2.clone())).await.unwrap();
        assert_eq!(resp.status, 400, "ssh_port=65536 应 400");
        b2["ssh_port"] = serde_json::json!(2222);
        b2["remote_port"] = serde_json::json!(0);
        let resp = h.handle(post_req(PATH_SSH_LIST, b2.clone())).await.unwrap();
        assert_eq!(resp.status, 400, "remote_port=0 应 400");
        // 模式非法
        let mut b3 = create_tunnel_req();
        b3["mode"] = serde_json::json!("socks");
        let resp = h.handle(post_req(PATH_SSH_LIST, b3)).await.unwrap();
        assert_eq!(resp.status, 400, "mode=socks 应 400");
        // local 缺 remote_host
        let mut b4 = create_tunnel_req();
        b4["remote_host"] = serde_json::Value::Null;
        let resp = h.handle(post_req(PATH_SSH_LIST, b4)).await.unwrap();
        assert_eq!(resp.status, 400, "local 缺 remote_host 应 400");
        // local_bind 格式非法
        let mut b5 = create_tunnel_req();
        b5["local_bind"] = serde_json::json!("127.0.0.1");
        let resp = h.handle(post_req(PATH_SSH_LIST, b5)).await.unwrap();
        assert_eq!(resp.status, 400, "local_bind 缺端口应 400");
        // password 字段红线
        let mut b6 = create_tunnel_req();
        b6["password"] = serde_json::json!("secret");
        let resp = h.handle(post_req(PATH_SSH_LIST, b6)).await.unwrap();
        assert_eq!(resp.status, 400, "password 字段应被拒绝");
        // 合法 dynamic（无 remote_*）→ 201
        let mut b7 = serde_json::json!({
            "name": "socks", "ssh_host": "h", "ssh_user": "u",
            "mode": "dynamic", "local_bind": "0.0.0.0:1080",
        });
        b7["remote_host"] = serde_json::Value::Null;
        let resp = h.handle(post_req(PATH_SSH_LIST, b7)).await.unwrap();
        assert_eq!(resp.status, 201, "合法 dynamic 应 201");
    }

    // 4. start spawn 失败降级 failed 不 panic（/bin/false 必失败 + 不存在的二进制）
    #[tokio::test]
    async fn ssh_start_failed_degrades_without_panic() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        std::env::set_var("NEXOS_SSH_BIN", "/bin/false");
        let h = empty_handler();
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "失败也应 200 返回降级状态（不 panic）");
        assert_eq!(resp.body["status"], "failed");
        assert!(resp.body["error"].as_str().unwrap().contains("退出"));
        assert!(resp.body["pid"].is_null());
        // 二进制不存在 → spawn Err 同样降级 failed
        std::env::set_var("NEXOS_SSH_BIN", "/nonexistent/ssh-xyz");
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "failed");
        assert!(resp.body["error"].as_str().unwrap().contains("未找到"));
        std::env::remove_var("NEXOS_SSH_BIN");
    }

    // 5. start 存活 → running + pid；stop → stopped + pid 清空（用脚本模拟 ssh 存活）
    #[cfg(unix)]
    #[tokio::test]
    async fn ssh_start_alive_then_stop() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let dir = std::env::temp_dir().join(format!("nexos-fwd-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-ssh.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 300\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&script).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&script, perm).unwrap();
        }
        std::env::set_var("NEXOS_SSH_BIN", script.to_str().unwrap());
        let h = empty_handler();
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // start → running
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "running", "脚本存活应 running");
        let pid = resp.body["pid"].as_u64().expect("running 应有 pid");
        assert!(pid > 0);
        assert!(resp.body["last_started"].is_string());
        // 幂等：再次 start 直接返回当前态（同一 pid，不重复 spawn）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "running");
        assert_eq!(resp.body["pid"].as_u64(), Some(pid), "幂等 start 不换 pid");
        // 详情实时探测 → running
        let resp = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "running");
        // stop → stopped + pid 清空
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "stopped");
        assert!(resp.body["pid"].is_null());
        // 停止后再 stop（幂等）与详情均 stopped
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "stopped");
        let resp = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "stopped");
        assert!(h.children.lock().unwrap().is_empty(), "Child 句柄应已回收");
        std::env::remove_var("NEXOS_SSH_BIN");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 6. stop 语义：停一条从未启动的隧道 → stopped 不报错；未知 id → 404
    #[tokio::test]
    async fn ssh_stop_semantics() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "stopped");
        // 未知 id
        let resp = h
            .handle(post_req(
                "/api/v1/forwarding/ssh/no-such-id/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let resp = h
            .handle(post_req(
                "/api/v1/forwarding/ssh/no-such-id/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 7. 存活探测 stale 逻辑：DB 里 running + 已死 pid（模拟 os-api 重启后的
    //    遗留记录）→ GET 详情探测到 stale → stopped + pid 清空
    #[cfg(unix)]
    #[tokio::test]
    async fn ssh_stale_detection_on_detail() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 造一个确定已死且已被收尸（非僵尸）的 pid：spawn true 并 wait
        let dead = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead.id();
        let _ = dead.wait_with_output();
        // 直接写 DB：模拟上一进程遗留的 running 记录
        {
            let conn = h.db.lock().unwrap();
            conn.execute(
                "UPDATE ssh_tunnels SET status='running', pid=?1 WHERE id=?2",
                params![i64::from(dead_pid), id],
            )
            .unwrap();
        }
        let resp = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(
            resp.body["status"], "stopped",
            "stale running 应降级 stopped"
        );
        assert!(resp.body["pid"].is_null(), "stale 后 pid 应清空");
        assert!(resp.body["error"].as_str().unwrap().contains("stale"));
        // 状态已回写 DB
        assert_eq!(h.ssh_tunnels_snapshot()[0].status, "stopped");
    }

    // 8. build_ssh_args 纯函数：-o 选项全齐 + 三种模式参数 + key/port/user@host
    #[test]
    fn build_ssh_args_covers_all_modes() {
        let base = SshTunnel {
            id: "t1".into(),
            name: "n".into(),
            ssh_host: "nas.local".into(),
            ssh_port: 2222,
            ssh_user: "oem".into(),
            private_key_path: None,
            mode: "local".into(),
            local_bind: "127.0.0.1:8080".into(),
            remote_host: Some("10.0.0.5".into()),
            remote_port: Some(80),
            autostart: false,
            watchdog: false,
            status: "stopped".into(),
            pid: None,
            error: None,
            created_at: "t".into(),
            last_started: None,
            restart_count: 0,
            last_restart_at: None,
        };
        let local = build_ssh_args(&base).join(" ");
        assert!(local.contains("-N"));
        for opt in [
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "ServerAliveInterval=30",
            "StrictHostKeyChecking=accept-new",
        ] {
            assert!(local.contains(opt), "缺 -o {opt}: {local}");
        }
        assert!(local.contains("-i ~/.ssh/id_ed25519"), "默认私钥: {local}");
        assert!(local.contains("-p 2222"), "缺 -p 端口: {local}");
        assert!(
            local.contains("-L 127.0.0.1:8080:10.0.0.5:80"),
            "缺 -L: {local}"
        );
        assert!(
            local.ends_with("oem@nas.local"),
            "缺 user@host 尾参: {local}"
        );
        // remote 模式 → -R
        let mut r = base.clone();
        r.mode = "remote".into();
        let remote = build_ssh_args(&r).join(" ");
        assert!(
            remote.contains("-R 127.0.0.1:8080:10.0.0.5:80"),
            "缺 -R: {remote}"
        );
        // dynamic 模式 → -D（无 remote 目标）
        let mut d = base.clone();
        d.mode = "dynamic".into();
        d.remote_host = None;
        d.remote_port = None;
        let dyn_args = build_ssh_args(&d);
        let joined = dyn_args.join(" ");
        assert!(joined.contains("-D 127.0.0.1:8080"), "缺 -D: {joined}");
        assert!(!dyn_args.iter().any(|a| a == "-L" || a == "-R"));
        // 显式私钥透传
        let mut k = base;
        k.private_key_path = Some("/etc/ssh/keys/prod".into());
        assert!(build_ssh_args(&k)
            .join(" ")
            .contains("-i /etc/ssh/keys/prod"));
    }

    // 9. RDP CRUD roundtrip（默认 3389 / 列表 / 删除）
    #[tokio::test]
    async fn rdp_crud_roundtrip() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(
                PATH_RDP_LIST,
                serde_json::json!({
                    "name": "办公 Windows",
                    "target_host": "192.168.1.50",
                    "listen_port": 33900,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["target_port"], 3389, "target_port 默认 3389");
        assert_eq!(resp.body["listen_port"], 33900);
        assert_eq!(resp.body["status"], "stopped");
        assert_eq!(resp.body["connections"], 0);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let list = h.handle(get_req(PATH_RDP_LIST)).await.unwrap();
        assert_eq!(list.body.as_array().unwrap().len(), 1);
        let del = h
            .handle(del_req(&format!("/api/v1/forwarding/rdp/{id}")))
            .await
            .unwrap();
        assert_eq!(del.status, 200);
        assert!(h.rdp_forwards_snapshot().is_empty());
        let gone = h
            .handle(del_req(&format!("/api/v1/forwarding/rdp/{id}")))
            .await
            .unwrap();
        assert_eq!(gone.status, 404);
    }

    // 10. RDP 创建校验：listen_port 0 / 越界 65536、target_host 空、name 空 → 400
    #[tokio::test]
    async fn rdp_create_validation() {
        let h = empty_handler();
        for (patch, why) in [
            (serde_json::json!({ "listen_port": 0 }), "listen_port=0"),
            (
                serde_json::json!({ "listen_port": 65536 }),
                "listen_port=65536",
            ),
            (
                serde_json::json!({ "target_port": 70000 }),
                "target_port=70000",
            ),
            (serde_json::json!({ "target_host": "" }), "target_host 空"),
            (serde_json::json!({ "name": " " }), "name 空"),
        ] {
            let mut body = serde_json::json!({
                "name": "win", "target_host": "192.168.1.50", "listen_port": 33901,
            });
            for (k, v) in patch.as_object().unwrap() {
                body[k] = v.clone();
            }
            let resp = h.handle(post_req(PATH_RDP_LIST, body)).await.unwrap();
            assert_eq!(resp.status, 400, "{why} 应 400");
        }
    }

    // 11. RDP start 端口冲突：端口被占 → error 状态带原因（不 panic），无代理任务残留
    #[tokio::test]
    async fn rdp_start_port_conflict_degrades_to_error() {
        let h = empty_handler();
        let port = unique_free_port().await;
        let holder = tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .unwrap();
        let resp = h
            .handle(post_req(
                PATH_RDP_LIST,
                serde_json::json!({
                    "name": "冲突", "target_host": "192.168.1.50", "listen_port": port,
                }),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/rdp/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "端口冲突应 200 降级 error（不 panic）");
        assert_eq!(resp.body["status"], "error");
        assert!(
            resp.body["error"].as_str().unwrap().contains("绑定失败"),
            "error 应带原因: {}",
            resp.body["error"]
        );
        assert!(h.proxies.lock().unwrap().is_empty(), "失败不应留代理任务");
        drop(holder);
        // 端口释放后再 start → running（并行测试可能瞬抢端口，短暂重试容忍）
        let mut status = String::new();
        for _ in 0..20 {
            let resp = h
                .handle(post_req(
                    &format!("/api/v1/forwarding/rdp/{id}/start"),
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            status = resp.body["status"].as_str().unwrap_or("").to_string();
            if status == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(status, "running", "端口释放后应可启动");
        // stop 收尾，避免拖住测试进程
        let _ = h
            .handle(post_req(
                &format!("/api/v1/forwarding/rdp/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await;
    }

    // 12. RDP 代理数据面（本机回环）：客户端 → 代理 → echo 目标 双向拷贝 + 连接计数
    #[tokio::test]
    async fn rdp_proxy_data_plane_and_connection_count() {
        let h = empty_handler();
        // echo 假 RDP 目标（127.0.0.1 随机端口）
        let echo_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = echo_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = echo_listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 64];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let _ = s.write_all(&buf[..n]).await;
            }
        });
        let listen_port = unique_free_port().await;
        let resp = h
            .handle(post_req(
                PATH_RDP_LIST,
                serde_json::json!({
                    "name": "回环测试", "target_host": "127.0.0.1",
                    "target_port": target_port, "listen_port": listen_port,
                }),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // start
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/rdp/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "running");
        // 客户端经代理读写
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", listen_port))
            .await
            .expect("代理监听应可达");
        client.write_all(b"ping-rdp").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("读不应超时")
            .unwrap_or(0);
        assert_eq!(&buf[..n], b"ping-rdp", "数据应经代理双向拷贝回显");
        // 连接计数（轮询 DB 落库）
        let mut connections = 0u64;
        for _ in 0..40 {
            connections = h
                .rdp_forwards_snapshot()
                .first()
                .map(|f| f.connections)
                .unwrap_or(0);
            if connections == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(connections, 1, "accept 计数应为 1");
        // stop → stopped
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/rdp/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "stopped");
        assert!(h.proxies.lock().unwrap().is_empty(), "代理任务应已 abort");
        // 停止后端口应释放（可再 bind；给运行时一个调度节拍防微竞态）
        let mut rebound = false;
        for _ in 0..20 {
            if tokio::net::TcpListener::bind(("0.0.0.0", listen_port))
                .await
                .is_ok()
            {
                rebound = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(rebound, "stop 后监听端口应释放");
    }

    // 13. rdp-file 内容断言：full address = Host 头（去端口）:listen_port、
    //     username 行有无、Content-Disposition/Content-Type、404
    #[tokio::test]
    async fn rdp_file_content_and_headers() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(
                PATH_RDP_LIST,
                serde_json::json!({
                    "name": "我的 Win11", "target_host": "192.168.1.50", "listen_port": 33900,
                }),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 带 Host 头 + username
        let resp = h
            .handle(get_req_with_host(
                &format!("/api/v1/forwarding/rdp/{id}/rdp-file?username=alice%40corp"),
                "192.168.1.10:8080",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers["content-type"], "application/rdp");
        assert_eq!(
            resp.headers["content-disposition"], "attachment; filename=\"我的-Win11.rdp\"",
            "文件名取 name 并净化"
        );
        let body = resp.body.as_str().unwrap();
        assert!(
            body.contains("full address:s:192.168.1.10:33900"),
            "full address 应取 Host 头（去端口）+ listen_port:\n{body}"
        );
        assert!(
            body.contains("username:s:alice@corp"),
            "username 行:\n{body}"
        );
        assert!(body.contains("screen mode id:i:2"), "合理默认项:\n{body}");
        assert!(
            body.contains("smart sizing"),
            "smart sizing 注释行:\n{body}"
        );
        // 无 username → 省略该行；无 Host 头 → 回退 hostname（格式可解析即可）
        let resp = h
            .handle(get_req(&format!(
                "/api/v1/forwarding/rdp/{id}/rdp-file?username="
            )))
            .await
            .unwrap();
        let body = resp.body.as_str().unwrap();
        assert!(
            !body.contains("username:s:"),
            "空 username 应省略该行:\n{body}"
        );
        assert!(
            {
                // 回退 hostname：行形如 full address:s:<非空主机>:33900
                let line = body
                    .lines()
                    .find(|l| l.starts_with("full address:s:"))
                    .unwrap();
                let rest = line.trim_start_matches("full address:s:");
                rest.ends_with(":33900") && rest.len() > ":33900".len()
            },
            "无 Host 头应回退 cached_hostname:\n{body}"
        );
        // 未知 id → 404
        let resp = h
            .handle(get_req("/api/v1/forwarding/rdp/no-such/rdp-file"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 14. stats 聚合：两类总数/运行数 + RDP 累计连接
    #[tokio::test]
    async fn stats_aggregation() {
        let h = empty_handler();
        // 2 条 SSH（1 running 走 DB 直写模拟）+ 1 条 RDP stopped
        for i in 0..2 {
            let mut body = create_tunnel_req();
            body["name"] = serde_json::json!(format!("t{i}"));
            h.handle(post_req(PATH_SSH_LIST, body)).await.unwrap();
        }
        {
            let conn = h.db.lock().unwrap();
            conn.execute(
                "UPDATE ssh_tunnels SET status='running' WHERE name='t0'",
                [],
            )
            .unwrap();
        }
        h.handle(post_req(
            PATH_RDP_LIST,
            serde_json::json!({ "name": "win", "target_host": "192.168.1.50", "listen_port": 33900 }),
        ))
        .await
        .unwrap();
        let resp = h.handle(get_req(PATH_STATS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ssh_tunnels_total"], 2);
        assert_eq!(resp.body["ssh_tunnels_running"], 1);
        assert_eq!(resp.body["rdp_forwards_total"], 1);
        assert_eq!(resp.body["rdp_forwards_running"], 0);
        assert_eq!(resp.body["rdp_total_connections"], 0);
    }

    // 15. 路由归属兜底：未匹配路径 404 + JSON error（不panic/不 Err）
    #[tokio::test]
    async fn unmatched_routes_fall_to_404() {
        let h = empty_handler();
        let resp = h
            .handle(get_req("/api/v1/forwarding/ssh/a/b/c"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body["error"], "forwarding: 未匹配的路由");
        let resp = h
            .handle(post_req(
                "/api/v1/forwarding/unknown",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 16. resume_autostart：autostart=true 的隧道在恢复时被尝试 start（失败降级
    //     failed 不 panic），stale running 先归 stopped
    #[tokio::test]
    async fn resume_autostart_starts_marked_tunnels() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        std::env::set_var("NEXOS_SSH_BIN", "/bin/false");
        let h = empty_handler();
        // 一条 autostart（start 必失败 → failed）+ 一条普通隧道（不动）
        let mut body = create_tunnel_req();
        body["autostart"] = serde_json::json!(true);
        let r1 = h.handle(post_req(PATH_SSH_LIST, body)).await.unwrap();
        let id1 = r1.body["id"].as_str().unwrap().to_string();
        let r2 = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        let id2 = r2.body["id"].as_str().unwrap().to_string();
        // 模拟上一进程遗留 running 态（id2 非 autostart：恢复后应归 stopped）
        {
            let conn = h.db.lock().unwrap();
            conn.execute(
                "UPDATE ssh_tunnels SET status='running', pid=999999 WHERE id=?1",
                params![id2],
            )
            .unwrap();
        }
        h.resume_autostart().await; // 不应 panic
        let snap = h.ssh_tunnels_snapshot();
        let t1 = snap.iter().find(|t| t.id == id1).unwrap();
        let t2 = snap.iter().find(|t| t.id == id2).unwrap();
        assert_eq!(
            t1.status, "failed",
            "autostart 尝试 start（false 必败降级）"
        );
        assert_eq!(
            t2.status, "stopped",
            "非 autostart 的 stale running 应归 stopped"
        );
        std::env::remove_var("NEXOS_SSH_BIN");
    }

    // =========================================================================
    // watchdog（运行期自愈）单元测——全部经 NEXOS_SSH_BIN 注入假 ssh：
    // 存活脚本 `exec sleep 300` / 必败 `/bin/false`，不起真实网络
    // =========================================================================

    /// NEXOS_FORWARDING_WATCHDOG_SECS 触碰互斥（与 SSH_BIN_LOCK 同款思路；
    /// 仅 W6/W7 两测试 set/remove 该变量）。
    static WATCHDOG_SECS_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    /// 写一个"存活"假 ssh 脚本（exec sleep 300），返回脚本路径。
    /// 调用方负责 NEXOS_SSH_BIN 环境变量与清理。
    fn write_fake_alive_ssh() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-fwd-wd-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-ssh.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 300\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&script).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&script, perm).unwrap();
        }
        script
    }

    /// 创建一条 watchdog=true 隧道并 start 到 running，返回 (handler 复用的 id, pid)。
    async fn start_watchdog_tunnel(h: &ForwardingRouteHandler) -> (String, u32) {
        let mut body = create_tunnel_req();
        body["watchdog"] = serde_json::json!(true);
        let resp = h.handle(post_req(PATH_SSH_LIST, body)).await.unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "running", "前置：隧道应先 running");
        let pid = resp.body["pid"].as_u64().expect("running 应有 pid") as u32;
        (id, pid)
    }

    // W1. 进程死后被拉起：watchdog=true + running + kill 进程 → 单轮扫描后
    //     重新 running（换 pid）、restart_count=1、last_restart_at 落库、详情可见
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_restarts_dead_tunnel_and_counts() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let script = write_fake_alive_ssh();
        std::env::set_var("NEXOS_SSH_BIN", script.to_str().unwrap());
        let h = empty_handler();
        let (id, pid) = start_watchdog_tunnel(&h).await;
        // 杀掉 ssh 进程（模拟远端断连/崩溃），给一点时间让信号送达
        kill_pid(pid);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        h.watchdog_scan_once().await;
        let snap = h.ssh_tunnels_snapshot();
        let t = snap.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.status, "running", "watchdog 应拉起死亡隧道");
        assert_ne!(t.pid, Some(pid), "拉起后应换新 pid（旧 pid: {pid}）");
        assert_eq!(t.restart_count, 1, "重启计数应为 1");
        assert!(
            t.last_restart_at.as_deref().is_some_and(|s| !s.is_empty()),
            "last_restart_at 应落库"
        );
        // 详情端点暴露重启计数器 {restart_count, last_restart_at}
        let detail = h
            .handle(get_req(&format!("/api/v1/forwarding/ssh/{id}")))
            .await
            .unwrap();
        assert_eq!(detail.body["restart_count"], 1);
        assert!(detail.body["last_restart_at"].is_string());
        // 再死一次 → 计数继续累计（且失败计数已在上次成功时清零）
        kill_pid(t.pid.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        h.watchdog_scan_once().await;
        let t = h
            .ssh_tunnels_snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(t.status, "running");
        assert_eq!(t.restart_count, 2, "第二次拉起应累计为 2");
        // 收尾：手动 stop 释放子进程
        let _ = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await;
        std::env::remove_var("NEXOS_SSH_BIN");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    // W2. 失败退避放弃：必败 /bin/false + 伪造"曾 running 已死"记录 →
    //     连续 5 次拉起失败后置 failed + error="watchdog 放弃：连续失败…"，
    //     第 6 轮不再尝试
    #[tokio::test]
    async fn watchdog_gives_up_after_consecutive_failures() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        std::env::set_var("NEXOS_SSH_BIN", "/bin/false");
        let h = empty_handler();
        let mut body = create_tunnel_req();
        body["watchdog"] = serde_json::json!(true);
        let resp = h.handle(post_req(PATH_SSH_LIST, body)).await.unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 伪造 os-api 运行期间 ssh 死亡：DB 记 running + 已死 pid（spawn true 并收尸）
        let dead = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead.id();
        let _ = dead.wait_with_output();
        {
            let conn = h.db.lock().unwrap();
            conn.execute(
                "UPDATE ssh_tunnels SET status='running', pid=?1 WHERE id=?2",
                params![i64::from(dead_pid), id],
            )
            .unwrap();
        }
        // 前 4 轮：拉起失败但仍在重试（failed 是 start 的降级态，计数在内存）
        for i in 1..WATCHDOG_MAX_FAILURES {
            h.watchdog_scan_once().await;
            let t = h
                .ssh_tunnels_snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap();
            assert_eq!(t.status, "failed", "第 {i} 轮拉起失败应降级 failed");
            assert!(
                !t.error.as_deref().unwrap_or("").contains("放弃"),
                "第 {i} 轮不应放弃: {:?}",
                t.error
            );
            assert_eq!(t.restart_count, 0);
        }
        // 第 5 轮：放弃
        h.watchdog_scan_once().await;
        let t = h
            .ssh_tunnels_snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(t.status, "failed", "放弃后应保持 failed");
        let err = t.error.as_deref().unwrap_or("");
        assert!(
            err.contains("watchdog 放弃：连续失败"),
            "放弃 error 应含标记词: {err}"
        );
        assert!(t.pid.is_none(), "放弃后应清 pid");
        assert_eq!(t.restart_count, 0, "从未成功不计重启");
        // 第 6 轮：不再尝试（无内存计数 + 非 running → 跳过，children 无残留）
        h.watchdog_scan_once().await;
        let t = h
            .ssh_tunnels_snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(t.status, "failed");
        assert!(
            t.error.as_deref().unwrap_or("").contains("watchdog 放弃"),
            "第 6 轮不应改动状态"
        );
        assert!(h.children.lock().unwrap().is_empty());
        std::env::remove_var("NEXOS_SSH_BIN");
    }

    // W3. 关 watchdog 不拉起：默认 watchdog=false + running + 进程死亡 →
    //     扫描完全不碰这条隧道（DB 仍 running + 旧 pid，restart_count=0）
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_disabled_tunnel_not_touched() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let script = write_fake_alive_ssh();
        std::env::set_var("NEXOS_SSH_BIN", script.to_str().unwrap());
        let h = empty_handler();
        // 默认创建（不带 watchdog 字段）→ false
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        assert_eq!(resp.body["watchdog"], false, "默认 watchdog=false");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let pid = resp.body["pid"].as_u64().unwrap() as u32;
        kill_pid(pid);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        h.watchdog_scan_once().await;
        let t = h
            .ssh_tunnels_snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(
            t.status, "running",
            "watchdog=false 扫描应完全跳过（留给详情探测标 stale）"
        );
        assert_eq!(t.pid, Some(pid), "pid 不应被改写");
        assert_eq!(t.restart_count, 0, "不应有重启计数");
        assert!(t.last_restart_at.is_none());
        std::env::remove_var("NEXOS_SSH_BIN");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    // W4. 手动 stop 永不拉起：watchdog=true + 用户 stop → 扫描保持 stopped
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_never_restarts_manually_stopped() {
        let _guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let script = write_fake_alive_ssh();
        std::env::set_var("NEXOS_SSH_BIN", script.to_str().unwrap());
        let h = empty_handler();
        let (id, _pid) = start_watchdog_tunnel(&h).await;
        let _ = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        h.watchdog_scan_once().await;
        let t = h
            .ssh_tunnels_snapshot()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap();
        assert_eq!(t.status, "stopped", "手动 stop 后 watchdog 不得拉起");
        assert_eq!(t.restart_count, 0);
        std::env::remove_var("NEXOS_SSH_BIN");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }

    // W5. 字段兼容：老请求体/老 DB 行缺 watchdog/restart_count/last_restart_at
    //     全部安全回退默认值；显式 watchdog=true 持久化可见；老库自动迁移补列
    #[tokio::test]
    async fn watchdog_field_compat_and_migration() {
        let h = empty_handler();
        // 老请求体（无 watchdog）→ false + 计数器默认值出现在响应里
        let resp = h
            .handle(post_req(PATH_SSH_LIST, create_tunnel_req()))
            .await
            .unwrap();
        assert_eq!(resp.body["watchdog"], false);
        assert_eq!(resp.body["restart_count"], 0);
        assert_eq!(resp.body["last_restart_at"], serde_json::Value::Null);
        // 显式 watchdog=true → 创建/列表/详情均可见
        let mut body = create_tunnel_req();
        body["watchdog"] = serde_json::json!(true);
        let resp = h.handle(post_req(PATH_SSH_LIST, body)).await.unwrap();
        let wid = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["watchdog"], true);
        let list = h.handle(get_req(PATH_SSH_LIST)).await.unwrap();
        let w = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == serde_json::json!(wid))
            .unwrap();
        assert_eq!(w["watchdog"], true, "watchdog 应持久化");
        // serde 老行兼容：DTO 缺三个新字段反序列化不炸、取默认
        let legacy = serde_json::json!({
            "id": "t-old", "name": "n", "ssh_host": "h", "ssh_user": "u",
            "local_bind": "127.0.0.1:8080", "created_at": "t"
        });
        let t: SshTunnel = serde_json::from_value(legacy).expect("老 JSON 应兼容");
        assert!(!t.watchdog);
        assert_eq!(t.restart_count, 0);
        assert!(t.last_restart_at.is_none());
        // 老库迁移：手工建 2026-08 之前的 16 列旧表 + 一行数据 → open 后补列可读写
        let tmp = std::env::temp_dir().join(format!("nexos-fwd-mig-{}.db", new_uuid()));
        {
            let old = Connection::open(&tmp).unwrap();
            old.execute_batch(
                "CREATE TABLE ssh_tunnels (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, ssh_host TEXT NOT NULL,
                    ssh_port INTEGER NOT NULL DEFAULT 22, ssh_user TEXT NOT NULL,
                    private_key_path TEXT, mode TEXT NOT NULL DEFAULT 'local',
                    local_bind TEXT NOT NULL, remote_host TEXT, remote_port INTEGER,
                    autostart INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'stopped',
                    pid INTEGER, error TEXT, created_at TEXT, last_started TEXT
                );
                INSERT INTO ssh_tunnels (id,name,ssh_host,ssh_user,local_bind,created_at)
                VALUES ('legacy-1','老隧道','10.0.0.9','root','127.0.0.1:9930','2026-07-01');",
            )
            .unwrap();
        }
        let h2 = ForwardingRouteHandler::with_db_path(tmp.to_str().unwrap());
        let snap = h2.ssh_tunnels_snapshot();
        assert_eq!(snap.len(), 1, "老行应读出");
        assert_eq!(snap[0].id, "legacy-1");
        assert!(!snap[0].watchdog, "迁移补列默认 false");
        assert_eq!(snap[0].restart_count, 0);
        assert!(snap[0].last_restart_at.is_none());
        // 迁移后的表可正常 update（watchdog 列已存在）
        {
            let conn = h2.db.lock().unwrap();
            conn.execute(
                "UPDATE ssh_tunnels SET watchdog=1, restart_count=3 WHERE id='legacy-1'",
                [],
            )
            .unwrap();
        }
        assert!(h2.ssh_tunnels_snapshot()[0].watchdog);
        assert_eq!(h2.ssh_tunnels_snapshot()[0].restart_count, 3);
        let _ = std::fs::remove_file(&tmp);
    }

    // W6. watchdog_secs 环境变量：合法覆写 / 非法与 <1 回退默认 30
    #[tokio::test]
    async fn watchdog_secs_env_default_and_override() {
        let _guard = WATCHDOG_SECS_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        assert_eq!(watchdog_secs(), 30, "未设置时默认 30");
        std::env::set_var("NEXOS_FORWARDING_WATCHDOG_SECS", "7");
        assert_eq!(watchdog_secs(), 7, "合法覆写");
        std::env::set_var("NEXOS_FORWARDING_WATCHDOG_SECS", "0");
        assert_eq!(watchdog_secs(), 30, "0 非法回退默认");
        std::env::set_var("NEXOS_FORWARDING_WATCHDOG_SECS", "abc");
        assert_eq!(watchdog_secs(), 30, "非数字回退默认");
        std::env::set_var("NEXOS_FORWARDING_WATCHDOG_SECS", " 12 ");
        assert_eq!(watchdog_secs(), 12, "容忍首尾空白");
        std::env::remove_var("NEXOS_FORWARDING_WATCHDOG_SECS");
        assert_eq!(watchdog_secs(), 30);
    }

    // W7. 后台循环端到端：spawn_watchdog（间隔 1s）+ 杀进程 → 轮询 DB 等自动拉起
    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_background_task_restarts_dead_tunnel() {
        let _ssh_guard = SSH_BIN_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _secs_guard = WATCHDOG_SECS_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let script = write_fake_alive_ssh();
        std::env::set_var("NEXOS_SSH_BIN", script.to_str().unwrap());
        std::env::set_var("NEXOS_FORWARDING_WATCHDOG_SECS", "1");
        let h = empty_handler();
        let (id, pid) = start_watchdog_tunnel(&h).await;
        h.spawn_watchdog();
        kill_pid(pid);
        // 轮询至多 ~8s：1s 间隔 + 拉起探测 800ms，留足裕量
        let mut restarted = false;
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let t = h
                .ssh_tunnels_snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .unwrap();
            if t.status == "running" && t.restart_count >= 1 && t.pid != Some(pid) {
                restarted = true;
                break;
            }
        }
        assert!(restarted, "后台 watchdog 循环应在进程死后自动拉起");
        let _ = h
            .handle(post_req(
                &format!("/api/v1/forwarding/ssh/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await;
        std::env::remove_var("NEXOS_SSH_BIN");
        std::env::remove_var("NEXOS_FORWARDING_WATCHDOG_SECS");
        let _ = std::fs::remove_dir_all(script.parent().unwrap());
    }
}
