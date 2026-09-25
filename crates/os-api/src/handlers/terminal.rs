//! `TerminalRouteHandler` —— 「管理」桌面应用（Web 终端：本地 shell + SSH 远程
//! 终端）的 HTTP 适配器，component = `terminal`，架构见 docs/ADMIN_CONSOLE.md。
//!
//! 用户定调（原话拆解）：「增加管理功能，与设置功能不冲突，管理功能得有 ssh
//! 终端，可以在打开终端」——独立的「管理」应用（不并入设置），含 SSH 终端与
//! 本地终端。
//!
//! # PTY 方案（portable-pty，纯 Rust）
//!
//! - **本地终端**：spawn `$SHELL`（缺省 `/bin/bash`）于 PTY，cwd=HOME；
//! - **SSH 终端**：spawn `ssh -tt -p <port> [-i key] [-o StrictHostKeyChecking=
//!   accept-new] user@host` 于 PTY——**PTY 下 ssh 的密码提示/交互全部透传**
//!   （密码认证也能用，用户在浏览器 xterm.js 里直接输，服务端不碰密码）；
//!   目标来源：provisioning 的 SSH targets（**只读复用**其注册表，
//!   `GET /provisioning/ssh/targets` 已有）或临时直连参数
//!   {host, port, user, key_path}。
//!
//! # WebSocket 帧协议（JSON 文本帧）
//!
//! 挂载 `/ws/terminal/{session_id}?token=<admin token>`（http.rs 同款 WS 升级
//! 模式，握手即验 admin token，失败 401——**终端是最高权限面，全端点 admin**）：
//!
//! - 客户端 → 服务端：`{"type":"input","data":"<base64>"}` /
//!   `{"type":"resize","cols":N,"rows":N}`
//! - 服务端 → 客户端：`{"type":"output","data":"<base64>"}` /
//!   `{"type":"exit","code":N}` / `{"type":"error","msg":"..."}`
//!
//! 输出聚合节流（50ms 批量，超 64KB 立即冲刷）防高频小帧打爆 WS；PTY 读端
//! EOF → 取子进程退出码 → 广播 exit 帧 → 会话自清理。**会话与 WS 连接解耦**：
//! 浏览器断线重连可续用同一会话，显式关闭走 DELETE。
//!
//! # 资源限制与安全
//!
//! - 会话上限 8（防资源滥用），超限 429；
//! - 会话空闲 30 分钟自动回收（`spawn_idle_reaper` 后台任务）；
//! - admin 鉴权全端点（REST 走 `requires_auth` + roles=["admin"]，WS 走
//!   query token 与 `NEXOS_ADMIN_TOKEN` 精确匹配——两者同源）；
//! - ssh 参数 argv 直传（无 shell 拼接，注入面为零）；直连 `key_path` 限
//!   绝对路径（target_id 来源的路径由 provisioning 域负责，原样透传）；
//! - kill = SIGHUP 会话首进程（portable-pty ChildKiller）+ 关 PTY 主端
//!   （内核对前台进程组再发 SIGHUP）→ 读端 EOF → 会话清理，不泄漏。
//!
//! # 路由表（4 条）
//!
//! | method | path                                | 动作（全部 admin） |
//! |--------|-------------------------------------|--------------------|
//! | GET    | `/api/v1/terminal/sessions`         | 活跃会话列表 |
//! | POST   | `/api/v1/terminal/sessions`         | 创建会话（spawn PTY）→ 201 |
//! | DELETE | `/api/v1/terminal/sessions/:id`     | 删除会话（kill + 关 PTY）→ 204 |
//! | GET    | `/api/v1/terminal/node-snapshot`    | 节点常用状态快照（管理页顶部状态条） |
//!
//! # node-snapshot（2026-08-30，快捷命令面板配套）
//!
//! `GET /api/v1/terminal/node-snapshot` 一次性聚合管理页顶部状态条所需的节点
//! 概况：版本（update 的 `current_version_from_env` 同源）/ 在线时长 + 内存
//! 使用率 + 根分区使用率（monitor 的 /proc 与 df 读取函数复用）/ P2P 连接数
//! （main.rs 注入的 os-p2p Handle，未启用时 `p2p_connected: null`）。**只做
//! 聚合，无新增执行面**——前端点击状态条项目只是往 PTY 写对应快捷命令。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use once_cell::sync::Lazy;
use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize, SlavePty,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::handlers::provisioning::SshTarget;

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 同时存在的终端会话上限（防资源滥用，超限 POST 429）。
pub const MAX_SESSIONS: usize = 8;

/// 会话空闲回收阈值（无输入且无输出持续 30 分钟 → kill + 清理）。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 输出聚合节流窗口：PTY 输出按 50ms 批量合并成 output 帧，防高频小帧。
pub const OUTPUT_THROTTLE: Duration = Duration::from_millis(50);

/// 聚合缓冲硬上限：超过即不等窗口立即冲刷（防大输出（cat 大文件）内存堆积）。
pub const OUTPUT_FLUSH_CAP: usize = 64 * 1024;

/// 空闲回收后台巡检周期。
const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// output 帧广播通道容量（50ms/帧 ≈ 20 帧/s，1024 ≈ 51s 缓冲；慢消费者丢帧
/// 时收 Lagged 错误并继续——终端数据是流式最新语义，宁可丢旧不阻塞写端）。
const OUTPUT_CHANNEL_CAP: usize = 1024;

/// 缺省终端尺寸（对齐 xterm.js 常见窗口）。
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

// ----------------------------------------------------------------------------
// WS 帧协议（JSON 文本帧，serde tag = "type"）
// ----------------------------------------------------------------------------

/// 客户端 → 服务端帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientFrame {
    /// 终端输入（字节 base64；xterm.js onData → 编码发送）。
    Input { data: String },
    /// 窗口尺寸变化（xterm fit 插件 → cols/rows）。
    Resize { cols: u16, rows: u16 },
}

/// 服务端 → 客户端帧。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerFrame {
    /// 终端输出（聚合后的字节 base64）。
    Output { data: String },
    /// 子进程退出（PTY 读端 EOF 后取真实退出码；随后服务端关连接）。
    Exit { code: i32 },
    /// 协议/IO 错误提示（不关连接）。
    Error { msg: String },
}

/// base64 编码（WS 帧 data 字段；与 qr_transfer 同款 STANDARD 引擎）。
fn b64_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// base64 解码（错误转字符串，供 error 帧回传）。
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败: {e}"))
}

/// input 帧 data 字段解码（http.rs 的 WS 层调用；错误文案直通 error 帧）。
pub fn ws_input_decode(data: &str) -> Result<Vec<u8>, String> {
    b64_decode(data)
}

// ----------------------------------------------------------------------------
// 输出节流聚合（纯逻辑，时钟可注入便于测试）
// ----------------------------------------------------------------------------

/// 输出聚合缓冲：把窗口期内的多个小块合并成一个 output 帧。
///
/// 纯数据结构（不持时钟）：`take_due` 由调用方传入「距上次冲刷的时长」，
/// 单测直接注入时钟断言 50ms 窗口语义，不真等时间。
#[derive(Debug, Default)]
struct ThrottleBuf {
    buf: Vec<u8>,
}

impl ThrottleBuf {
    /// 追加一段 PTY 输出。
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 满足条件时取出聚合数据：非空 &&（距上次冲刷 ≥ 窗口 || 缓冲 ≥ 硬上限）。
    fn take_due(&mut self, since_flush: Duration, window: Duration, cap: usize) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            return None;
        }
        if since_flush >= window || self.buf.len() >= cap {
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        }
    }
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 活跃终端会话的观测信息（GET 列表 / POST 创建响应）。
#[derive(Debug, Clone, Serialize)]
pub struct TerminalSessionInfo {
    pub session_id: String,
    /// `"local"` | `"ssh"`
    pub kind: String,
    /// 展示目标：「本地 shell」/「root@10.0.0.2:22」/「<目标名>（user@host:port）」
    pub target: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at: String,
}

/// 创建会话请求体（POST /api/v1/terminal/sessions）。
#[derive(Debug, Deserialize)]
struct CreateTerminalSessionBody {
    /// `"local"` | `"ssh"`
    kind: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: Option<String>,
    /// 直连私钥路径（限绝对路径；None 时用 ssh 缺省密钥）。
    #[serde(default)]
    key_path: Option<String>,
    /// provisioning SSH 目标 id（提供时忽略直连参数，只读复用其注册表）。
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// `GET /api/v1/terminal/node-snapshot` 响应：节点常用状态快照（管理页顶部
/// 状态条数据源；点击状态条项目 = 前端往终端发对应快捷命令）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeSnapshot {
    /// 当前系统版本（与 /update/status 的 current_version 同源）
    pub version: String,
    /// 主机在线时长（秒，/proc/uptime）
    pub uptime_secs: u64,
    /// P2P 已连接节点数（None = P2P 未启用 NEXOS_P2P_ENABLE 未开）
    pub p2p_connected: Option<usize>,
    /// 根分区使用率（0-100，一位小数；读取失败 0）
    pub disk_use_pct: f32,
    /// 内存使用率（0-100，一位小数；used = total - available，与 monitor 同口径）
    pub mem_use_pct: f32,
}

/// 使用率百分比（0-100，一位小数；total=0 → 0 防除零）。
#[must_use]
pub fn use_pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let pct = used as f64 / total as f64 * 100.0;
    (((pct.clamp(0.0, 100.0)) * 10.0).round() / 10.0) as f32
}

// ----------------------------------------------------------------------------
// ssh 命令行组装（纯函数，argv 直传无 shell 拼接）
// ----------------------------------------------------------------------------

/// 组装 `ssh` 参数向量：`-tt -o StrictHostKeyChecking=accept-new -p <port>
/// [-i <key>] user@host`。
///
/// - `-tt`：强制分配远程 PTY（我们已在本地 PTY 里跑 ssh，远程交互程序同样
///   需要 tty 才能正常工作）；
/// - `accept-new`：首连自动接受新主机密钥（TOFU，与 provisioning SSH 同款）；
/// - **无 BatchMode**：刻意保留交互——密码提示经 PTY 透传到浏览器，密码认证
///   也能用（这是「管理」终端与 provisioning 无人值守部署的分界线）；
/// - 调用方以 argv 直传 spawn，不经 shell，无注入面。
#[must_use]
pub fn ssh_argv(host: &str, port: u16, user: &str, key_path: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "-tt".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-p".to_string(),
        port.to_string(),
    ];
    if let Some(key) = key_path.filter(|k| !k.trim().is_empty()) {
        argv.push("-i".to_string());
        argv.push(key.to_string());
    }
    argv.push(format!("{user}@{host}"));
    argv
}

// ----------------------------------------------------------------------------
// PTY 会话
// ----------------------------------------------------------------------------

/// 一个进行中的 PTY 会话：master（resize）+ writer（input）+ killer（SIGHUP）
/// + child（wait 退出码）+ 输出广播通道。
///
/// `ChildKiller` 与 `Child` 刻意分离持有（portable-pty 提供 `clone_killer`
/// 正是为此）：kill 不必等 `wait()` 让出锁——否则「wait 阻塞等进程退出 +
/// kill 等锁发信号」互等死锁。
pub struct PtySession {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub created_at: String,
    /// (cols, rows)，resize 帧更新。
    size: Mutex<(u16, u16)>,
    /// PTY 主端：resize 用；kill 时 take+drop（关主端 → 前台进程组收 SIGHUP
    /// → 读端 EOF）。`MasterPty: Send` 非 Sync，包 Mutex 共享。
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// PTY 写端（input 帧写入）。短临界区同步锁（write 是快速 syscall）。
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// 子进程句柄（wait 退出码专用；kill 走 killer，两者不互等）。
    child: Arc<tokio::sync::Mutex<Box<dyn Child + Send + Sync>>>,
    /// 独立杀手：SIGHUP 会话首进程，不与 wait 争锁。
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 输出/exit/error 帧广播（WS 客户端 + 测试订阅）。
    output_tx: broadcast::Sender<ServerFrame>,
    /// 最近活动时刻（输入或输出都算），空闲回收判定依据。
    last_active: Mutex<Instant>,
}

impl PtySession {
    /// 广播一帧（无订阅者时忽略发送错误）。
    fn broadcast(&self, frame: ServerFrame) {
        let _ = self.output_tx.send(frame);
    }

    /// 订阅输出帧流（WS 升级完成后调用；可多订阅者，断线重连续流）。
    pub fn subscribe(&self) -> broadcast::Receiver<ServerFrame> {
        self.output_tx.subscribe()
    }

    /// 观测快照。
    pub fn info(&self) -> TerminalSessionInfo {
        let (cols, rows) = *self.size.lock().expect("terminal size poisoned");
        TerminalSessionInfo {
            session_id: self.id.clone(),
            kind: self.kind.clone(),
            target: self.target.clone(),
            cols,
            rows,
            created_at: self.created_at.clone(),
        }
    }

    /// 写终端输入（input 帧）。同步阻塞写（少量字节，快速 syscall），
    /// 异步上下文由调用方包 `spawn_blocking`。
    pub fn write_input(&self, data: &[u8]) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        let mut w = self
            .writer
            .lock()
            .map_err(|e| format!("writer 锁中毒: {e}"))?;
        w.write_all(data)
            .and_then(|_| w.flush())
            .map_err(|e| format!("PTY 写入失败: {e}"))?;
        self.touch();
        Ok(())
    }

    /// 调整 PTY 尺寸（resize 帧；内核更新 winsize 并向子进程发 SIGWINCH）。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        let (cols, rows) = clamp_size(cols, rows);
        {
            let master = self
                .master
                .lock()
                .map_err(|e| format!("master 锁中毒: {e}"))?;
            let m = master.as_ref().ok_or_else(|| "PTY 已关闭".to_string())?;
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("PTY resize 失败: {e}"))?;
        }
        *self.size.lock().expect("terminal size poisoned") = (cols, rows);
        Ok(())
    }

    /// 终止会话：SIGHUP 会话首进程 + 关 PTY 主端（内核对前台进程组再发
    /// SIGHUP）→ 读端 EOF → 聚合任务发 exit 帧并自清理。同步 + 幂等
    /// （DELETE / 空闲回收 / 测试守卫三处复用，无 async 上下文依赖）。
    pub fn kill(&self) {
        {
            let mut killer = match self.killer.lock() {
                Ok(k) => k,
                Err(e) => e.into_inner(),
            };
            let _ = killer.kill();
        }
        // 关主端必须 take 出来 drop（MutexGuard 持有不影响析构）。
        let master = self.master.lock().map(|mut m| m.take()).ok().flatten();
        drop(master);
        // writer 换成 sink：向 slave 发 EOF，且后续 write_input 不阻塞在
        // 已关闭的 fd 上（会话终止后输入本就无意义）。
        if let Ok(mut w) = self.writer.lock() {
            *w = Box::new(std::io::sink());
        }
    }

    /// 最近活动距今的时长（空闲回收判定）。
    fn idle_for(&self) -> Duration {
        self.last_active
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// 刷新最近活动时刻。
    fn touch(&self) {
        if let Ok(mut t) = self.last_active.lock() {
            *t = Instant::now();
        }
    }

    /// 测试钩子：把 last_active 拨回过去（注入时钟——免真等 30 分钟）。
    #[cfg(test)]
    fn force_idle(&self, ago: Duration) {
        if let Ok(mut t) = self.last_active.lock() {
            if let Some(past) = Instant::now().checked_sub(ago) {
                *t = past;
            }
        }
    }
}

/// 终端尺寸合法范围收拢（对齐 xterm 常见窗口，防 0/巨幅值打爆内核）。
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 500), rows.clamp(2, 300))
}

// ----------------------------------------------------------------------------
// 会话注册表（TerminalSessions）
// ----------------------------------------------------------------------------

/// spawn 失败类型（REST 层映射 400/404/429/500；pub 因
/// `TerminalSessions::spawn_local/spawn_ssh` 是公开入口）。
#[derive(Debug)]
pub enum SpawnError {
    /// 会话数达上限 → 429。
    Limit,
    /// 参数非法（附原因）→ 400。
    Invalid(String),
    /// target_id 不在 provisioning 注册表 → 404。
    NotFound(String),
    /// PTY/子进程创建失败（附原因）→ 500。
    Spawn(String),
}

/// 全部终端会话的注册表：创建/列表/删除/空闲回收。REST handler 与 WS 升级
/// 层共享同一实例（`TerminalSessions::shared()` 进程级单例；测试用独立实例
/// 隔离）。spawn 系列要求 `&Arc<Self>`——聚合任务需回持注册表做 EOF 自清理。
pub struct TerminalSessions {
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
    counter: AtomicU64,
    max_sessions: usize,
    idle_timeout: Duration,
}

impl TerminalSessions {
    /// 全参构造（测试注入上限与空闲阈值）。
    #[must_use]
    pub fn with_limits(max_sessions: usize, idle_timeout: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            max_sessions,
            idle_timeout,
        }
    }

    /// 进程级共享实例（REST handler / WS 升级层 / main.rs 装配三方同源）。
    #[must_use]
    pub fn shared() -> Arc<Self> {
        static SHARED: Lazy<Arc<TerminalSessions>> =
            Lazy::new(|| Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT)));
        SHARED.clone()
    }

    /// 活跃会话列表（按创建顺序）。
    pub fn list(&self) -> Vec<TerminalSessionInfo> {
        let sessions = self.sessions.lock().expect("terminal sessions poisoned");
        let mut ids: Vec<&String> = sessions.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| sessions.get(id).map(|s| s.info()))
            .collect()
    }

    /// 取一个会话（WS 升级前校验存在性）。
    pub fn get(&self, id: &str) -> Option<Arc<PtySession>> {
        self.sessions
            .lock()
            .expect("terminal sessions poisoned")
            .get(id)
            .cloned()
    }

    /// 当前会话数。
    pub fn count(&self) -> usize {
        self.sessions
            .lock()
            .expect("terminal sessions poisoned")
            .len()
    }

    /// 创建本地终端会话：spawn `$SHELL`（缺省 `/bin/bash`）于 PTY，cwd=HOME。
    pub fn spawn_local(
        self: &Arc<Self>,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSessionInfo, SpawnError> {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/bash".to_string());
        let home = std::env::var("HOME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(home);
        self.spawn_session("local", "本地 shell".to_string(), cols, rows, cmd)
    }

    /// 创建 SSH 终端会话：spawn `ssh <argv>` 于 PTY（参数见 [`ssh_argv`]）。
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_ssh(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        user: &str,
        key_path: Option<&str>,
        label: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSessionInfo, SpawnError> {
        let mut cmd = CommandBuilder::new("ssh");
        for arg in ssh_argv(host, port, user, key_path) {
            cmd.arg(arg);
        }
        let target = label
            .map(|name| format!("{name}（{user}@{host}:{port}）"))
            .unwrap_or_else(|| format!("{user}@{host}:{port}"));
        self.spawn_session("ssh", target, cols, rows, cmd)
    }

    /// spawn 核心：上限检查 → openpty → spawn → 装配会话 → 起读端/聚合任务。
    fn spawn_session(
        self: &Arc<Self>,
        kind: &str,
        target: String,
        cols: u16,
        rows: u16,
        mut cmd: CommandBuilder,
    ) -> Result<TerminalSessionInfo, SpawnError> {
        let (cols, rows) = clamp_size(cols, rows);
        // 上限检查 + 插入在同一锁作用域（防并发创建越过上限）。
        let mut sessions = self.sessions.lock().expect("terminal sessions poisoned");
        if sessions.len() >= self.max_sessions {
            return Err(SpawnError::Limit);
        }

        // 统一终端环境：xterm-256color（本地与远程一致，颜色/控制序列对齐）。
        cmd.env("TERM", "xterm-256color");

        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| SpawnError::Spawn(format!("PTY 创建失败: {e}")))?;
        let child = SlavePty::spawn_command(&*pair.slave, cmd)
            .map_err(|e| SpawnError::Spawn(format!("子进程 spawn 失败: {e}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SpawnError::Spawn(format!("PTY 读端克隆失败: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| SpawnError::Spawn(format!("PTY 写端获取失败: {e}")))?;
        let killer = child.clone_killer();
        // 立即丢弃 slave 端：父进程持有 slave fd 会阻止读端 EOF 语义
        //（须由「所有 slave fd 已关 + 子进程退出」触发 EOF，否则聚合任务挂死）。
        drop(pair.slave);

        let id = format!("term-{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let (output_tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAP);
        let session = Arc::new(PtySession {
            id: id.clone(),
            kind: kind.to_string(),
            target,
            created_at: now_iso(),
            size: Mutex::new((cols, rows)),
            master: Mutex::new(Some(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(tokio::sync::Mutex::new(child)),
            killer: Mutex::new(killer),
            output_tx,
            last_active: Mutex::new(Instant::now()),
        });
        sessions.insert(id.clone(), session.clone());
        drop(sessions);

        // 读端任务（阻塞线程）：PTY 输出 → 无界通道（聚合任务消费）。
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF（子进程退出/PTY 关闭）
                    Ok(n) => {
                        if chunk_tx.send(buf[..n].to_vec()).is_err() {
                            break; // 聚合任务已退出
                        }
                    }
                }
            }
        });
        // 聚合任务：50ms 窗口批量合并 → output 帧；EOF → exit 帧 + 自清理。
        let registry = self.clone();
        let info = session.info();
        tokio::spawn(async move {
            run_output_aggregator(session, registry, chunk_rx).await;
        });
        Ok(info)
    }

    /// 删除会话（kill 进程 + 关 PTY + 移出注册表）。返回是否存在。同步幂等。
    pub fn kill_session(&self, id: &str) -> bool {
        let session = {
            let mut sessions = self.sessions.lock().expect("terminal sessions poisoned");
            sessions.remove(id)
        };
        match session {
            Some(s) => {
                s.kill();
                true
            }
            None => false,
        }
    }

    /// 空闲回收：对超过 `idle_timeout` 无输入无输出的会话执行 kill+清理。
    /// 返回回收数。
    pub fn reap_idle(&self) -> usize {
        let idle_ids: Vec<String> = {
            let sessions = self.sessions.lock().expect("terminal sessions poisoned");
            sessions
                .iter()
                .filter(|(_, s)| s.idle_for() >= self.idle_timeout)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let reaped = idle_ids.len();
        for id in &idle_ids {
            self.kill_session(id);
        }
        reaped
    }

    /// 启动空闲回收后台任务（60s 巡检一轮；main.rs 装配时调用一次）。
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        let sessions = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(IDLE_REAP_INTERVAL);
            loop {
                tick.tick().await;
                let reaped = sessions.reap_idle();
                if reaped > 0 {
                    eprintln!(
                        "[os-api][terminal] 空闲回收 {reaped} 个会话（{IDLE_TIMEOUT:?} 无活动）"
                    );
                }
            }
        });
    }

    /// 只在注册表内移除（不 kill）——聚合任务 EOF 自清理路径专用。
    fn remove_only(&self, id: &str) {
        self.sessions
            .lock()
            .expect("terminal sessions poisoned")
            .remove(id);
    }
}

/// 输出聚合任务：读端 chunk → 50ms 批量合并 → output 帧；通道关闭（EOF）→
/// flush 余量 → wait 退出码 → exit 帧 → 注册表自清理。
async fn run_output_aggregator(
    session: Arc<PtySession>,
    registry: Arc<TerminalSessions>,
    mut chunk_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let mut buf = ThrottleBuf::default();
    let mut last_flush = Instant::now();
    let mut tick = tokio::time::interval(OUTPUT_THROTTLE);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            chunk = chunk_rx.recv() => match chunk {
                Some(data) => {
                    buf.push(&data);
                    // 超硬上限不等窗口立即冲刷（大输出防内存堆积）。
                    if let Some(out) = buf.take_due(Duration::ZERO, OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP) {
                        session.touch();
                        session.broadcast(ServerFrame::Output { data: b64_encode(&out) });
                        last_flush = Instant::now();
                    }
                }
                None => {
                    // EOF：冲刷余量 → 取退出码 → exit 帧 → 自清理。
                    if let Some(out) = buf.take_due(Duration::from_secs(1), OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP) {
                        session.broadcast(ServerFrame::Output { data: b64_encode(&out) });
                    }
                    let code = wait_exit_code(&session).await;
                    session.broadcast(ServerFrame::Exit { code });
                    registry.remove_only(&session.id);
                    break;
                }
            },
            _ = tick.tick() => {
                if let Some(out) = buf.take_due(last_flush.elapsed(), OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP) {
                    session.touch();
                    session.broadcast(ServerFrame::Output { data: b64_encode(&out) });
                    last_flush = Instant::now();
                }
            }
        }
    }
}

/// 阻塞等待子进程退出码（signal 终止映射 1——portable-pty ExitStatus 语义；
/// spawn/wait 本身失败 -1）。
async fn wait_exit_code(session: &Arc<PtySession>) -> i32 {
    let child = session.child.clone();
    tokio::task::spawn_blocking(move || match child.blocking_lock().wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => -1,
    })
    .await
    .unwrap_or(-1)
}

/// 当前 ISO 时间戳（本地时区，与 provisioning 同款）。
fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// TerminalRouteHandler（REST 边界）
// ----------------------------------------------------------------------------

/// provisioning SSH 目标只读来源（main.rs 装配注入；测试注入桩）。
pub type SshTargetsProvider = Arc<dyn Fn() -> Vec<SshTarget> + Send + Sync>;

/// 「管理」桌面应用路由处理器——REST 会话生命周期 + PTY spawn + 节点状态快照。
pub struct TerminalRouteHandler {
    sessions: Arc<TerminalSessions>,
    targets: SshTargetsProvider,
    /// os-p2p Handle（main.rs 注入，node-snapshot 的 P2P 连接数来源；None =
    /// P2P 未启用 → p2p_connected: null）。与 node_view/transfer 同款共享 clone。
    p2p: Option<os_p2p::Handle>,
}

impl TerminalRouteHandler {
    /// 共享注册表构造（main.rs 装配：与 WS 升级层同一实例；无 SSH 目标源）。
    #[must_use]
    pub fn sharing_global_registry() -> Self {
        Self {
            sessions: TerminalSessions::shared(),
            targets: Arc::new(Vec::new),
            p2p: None,
        }
    }

    /// 独立注册表构造（测试隔离）。
    #[must_use]
    pub fn with_sessions(sessions: Arc<TerminalSessions>) -> Self {
        Self {
            sessions,
            targets: Arc::new(Vec::new),
            p2p: None,
        }
    }

    /// 注入 provisioning SSH 目标只读来源（builder）。
    #[must_use]
    pub fn with_ssh_targets(mut self, targets: SshTargetsProvider) -> Self {
        self.targets = targets;
        self
    }

    /// 注入 os-p2p Handle（builder；node-snapshot 的 P2P 连接数来源）。
    #[must_use]
    pub fn with_p2p_handle(mut self, handle: Option<os_p2p::Handle>) -> Self {
        self.p2p = handle;
        self
    }

    /// 会话注册表（main.rs 启动空闲回收任务用）。
    #[must_use]
    pub fn sessions(&self) -> &Arc<TerminalSessions> {
        &self.sessions
    }

    /// 解析 SSH 目标：target_id 优先（provisioning 注册表只读复用），
    /// 否则用直连参数（host 必填；key_path 限绝对路径）。
    /// 返回 (host, port, user, key_path, label)。
    #[allow(clippy::type_complexity)]
    fn resolve_ssh_params(
        &self,
        body: &CreateTerminalSessionBody,
    ) -> Result<(String, u16, String, Option<String>, Option<String>), SpawnError> {
        if let Some(target_id) = body.target_id.as_deref().filter(|s| !s.trim().is_empty()) {
            let target = (self.targets)()
                .into_iter()
                .find(|t| t.id == target_id)
                .ok_or_else(|| SpawnError::NotFound(format!("SSH 目标不存在: {target_id}")))?;
            return Ok((
                target.host,
                target.port,
                target.user,
                target.private_key_path,
                Some(target.name),
            ));
        }
        let host = body
            .host
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SpawnError::Invalid("ssh 会话需提供 host（或 target_id）".into()))?
            .to_string();
        let user = body
            .user
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_string();
        let key_path = body
            .key_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // 安全红线：直连 key_path 限绝对路径（防相对路径被 ssh 按服务端 cwd
        // 解析到意外位置；~/.ssh/... 也拒绝——shell 展开语义在这里不存在）。
        if let Some(key) = key_path.as_deref() {
            if !key.starts_with('/') {
                return Err(SpawnError::Invalid(format!("key_path 须为绝对路径: {key}")));
            }
        }
        Ok((host, body.port.unwrap_or(22), user, key_path, None))
    }
}

impl Default for TerminalRouteHandler {
    fn default() -> Self {
        Self::with_sessions(Arc::new(TerminalSessions::with_limits(
            MAX_SESSIONS,
            IDLE_TIMEOUT,
        )))
    }
}

#[async_trait]
impl RouteHandler for TerminalRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/terminal/sessions"),
            spec(HttpMethod::Post, "/api/v1/terminal/sessions"),
            spec(HttpMethod::Delete, "/api/v1/terminal/sessions/:id"),
            spec(HttpMethod::Get, "/api/v1/terminal/node-snapshot"),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/terminal/sessions —— 活跃会话列表（admin）
            (HttpMethod::Get, ["api", "v1", "terminal", "sessions"]) => {
                let list = self.sessions.list();
                Ok(ok_json(serde_json::to_value(&list).map_err(|e| {
                    ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
                })?))
            }

            // —— POST /api/v1/terminal/sessions —— 创建会话（admin，spawn PTY）
            (HttpMethod::Post, ["api", "v1", "terminal", "sessions"]) => {
                let body: CreateTerminalSessionBody =
                    serde_json::from_value(req.body).map_err(|e| {
                        ApiGatewayError::Internal(format!("解析终端会话请求体失败: {e}"))
                    })?;
                let cols = body.cols.unwrap_or(DEFAULT_COLS);
                let rows = body.rows.unwrap_or(DEFAULT_ROWS);
                let spawn_result: Result<TerminalSessionInfo, SpawnError> = match body.kind.as_str()
                {
                    "local" => self.sessions.spawn_local(cols, rows),
                    "ssh" => match self.resolve_ssh_params(&body) {
                        Ok((host, port, user, key_path, label)) => self.sessions.spawn_ssh(
                            &host,
                            port,
                            &user,
                            key_path.as_deref(),
                            label,
                            cols,
                            rows,
                        ),
                        Err(e) => Err(e),
                    },
                    other => Err(SpawnError::Invalid(format!(
                        "kind 仅支持 local / ssh，收到 {other:?}"
                    ))),
                };
                match spawn_result {
                    Ok(info) => Ok(ApiResponse {
                        status: 201,
                        body: serde_json::to_value(&info).map_err(|e| {
                            ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
                        })?,
                        headers: serde_json::json!({}),
                    }),
                    Err(SpawnError::Limit) => Ok(error_response(
                        429,
                        &format!(
                            "终端会话已达上限 {}（先关闭闲置会话）",
                            self.sessions.max_sessions
                        ),
                    )),
                    Err(SpawnError::Invalid(msg)) => Ok(error_response(400, &msg)),
                    Err(SpawnError::NotFound(msg)) => Ok(error_response(404, &msg)),
                    Err(SpawnError::Spawn(msg)) => Ok(error_response(500, &msg)),
                }
            }

            // —— DELETE /api/v1/terminal/sessions/:id —— 删除会话（admin）
            (HttpMethod::Delete, ["api", "v1", "terminal", "sessions", id]) => {
                if self.sessions.kill_session(id) {
                    Ok(ApiResponse {
                        status: 204,
                        body: serde_json::Value::Null,
                        headers: serde_json::json!({}),
                    })
                } else {
                    Ok(error_response(404, &format!("终端会话不存在: {id}")))
                }
            }

            // —— GET /api/v1/terminal/node-snapshot —— 节点状态快照（admin）
            // 聚合：版本（update 同源）+ uptime/内存/磁盘（monitor /proc + df
            // 读取函数复用）+ P2P 连接数（注入的 Handle）。只读聚合，无执行面。
            (HttpMethod::Get, ["api", "v1", "terminal", "node-snapshot"]) => {
                let p2p_connected = match &self.p2p {
                    Some(handle) => {
                        Some(handle.peers().await.iter().filter(|p| p.connected).count())
                    }
                    None => None,
                };
                // /proc + df（子进程）读取全部丢 spawn_blocking 池，不阻塞 reactor。
                let (uptime_secs, mem_use_pct, disk_use_pct) = tokio::task::spawn_blocking(|| {
                    let (mem_total, mem_avail, _, _) = crate::handlers::monitor::read_meminfo();
                    let mem_used = mem_total.saturating_sub(mem_avail);
                    let (disk_total, disk_used) = crate::handlers::monitor::read_root_disk();
                    (
                        crate::handlers::monitor::read_uptime(),
                        use_pct(mem_used, mem_total),
                        use_pct(disk_used, disk_total),
                    )
                })
                .await
                .unwrap_or((0, 0.0, 0.0));
                let snap = NodeSnapshot {
                    version: crate::handlers::update::current_version_from_env(),
                    uptime_secs,
                    p2p_connected,
                    disk_use_pct,
                    mem_use_pct,
                };
                Ok(ok_json(serde_json::to_value(&snap).map_err(|e| {
                    ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
                })?))
            }

            _ => Ok(error_response(404, "terminal: 未匹配的路由")),
        }
    }
}

/// 构造一条 RouteSpec（component 固定 `terminal`，全部 admin）。
fn spec(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "terminal".to_string(),
        // 安全红线：终端 = 最高权限面（等效 root shell），全部端点 admin。
        requires_auth: true,
        required_roles: vec!["admin".to_string()],
    }
}

/// 构造一个 200 JSON 响应。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 从请求路径剥离 query 后的纯 path 段。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    /// 订阅会话输出，轮询直到解拼后的累计输出包含 needle（超时返回已累计的）。
    async fn collect_until_contains(
        rx: &mut broadcast::Receiver<ServerFrame>,
        needle: &str,
        timeout: Duration,
    ) -> String {
        let deadline = Instant::now() + timeout;
        let mut acc = String::new();
        while Instant::now() < deadline {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), rx.recv()).await
            {
                Ok(Ok(ServerFrame::Output { data })) => {
                    if let Ok(bytes) = b64_decode(&data) {
                        acc.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    if acc.contains(needle) {
                        return acc;
                    }
                }
                // exit/error 帧：返回已累计内容（外层断言 needle 失败即暴露）
                Ok(Ok(ServerFrame::Exit { .. })) | Ok(Ok(ServerFrame::Error { .. })) => {
                    return acc;
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => break, // 超时
            }
        }
        acc
    }

    // —— 1. ssh 命令行组装（纯函数）：-tt / -o / -p / -i / user@host ——
    #[test]
    fn ssh_argv_assembles_flags_port_key_and_destination() {
        let argv = ssh_argv("10.0.0.9", 2222, "root", Some("/home/oem/.ssh/id_ed25519"));
        assert_eq!(
            argv,
            vec![
                "-tt",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-p",
                "2222",
                "-i",
                "/home/oem/.ssh/id_ed25519",
                "root@10.0.0.9",
            ]
        );
        // 无 key_path：不出现 -i
        let argv = ssh_argv("h", 22, "u", None);
        assert!(!argv.contains(&"-i".to_string()));
        assert_eq!(argv.last().unwrap(), "u@h");
    }

    // —— 2. WS 帧编解码：input/output/resize/exit JSON 往返 ——
    #[test]
    fn ws_frame_json_codec_roundtrip() {
        let input = ClientFrame::Input {
            data: b64_encode(b"echo hi\n"),
        };
        let json = serde_json::to_string(&input).unwrap();
        assert_eq!(json, r#"{"type":"input","data":"ZWNobyBoaQo="}"#);
        let parsed: ClientFrame = serde_json::from_str(&json).unwrap();
        match parsed {
            ClientFrame::Input { data } => assert_eq!(b64_decode(&data).unwrap(), b"echo hi\n"),
            other => panic!("应为 Input 帧: {other:?}"),
        }

        let resize = ClientFrame::Resize {
            cols: 120,
            rows: 40,
        };
        let parsed: ClientFrame =
            serde_json::from_str(&serde_json::to_string(&resize).unwrap()).unwrap();
        assert!(matches!(
            parsed,
            ClientFrame::Resize {
                cols: 120,
                rows: 40
            }
        ));

        let frames = vec![
            ServerFrame::Output {
                data: b64_encode(&[0x1b, 0x5b, 0x30, 0x6d]),
            },
            ServerFrame::Exit { code: 127 },
            ServerFrame::Error {
                msg: "PTY 已关闭".into(),
            },
        ];
        for f in frames {
            let parsed: ServerFrame =
                serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
            assert_eq!(parsed, f);
        }
        // 未知 type 解析失败（协议收紧）
        assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"nope"}"#).is_err());
    }

    // —— 3. 输出节流聚合（注入时钟）：窗口内合并、到期冲刷、超上限立即冲 ——
    #[test]
    fn throttle_buf_batches_within_window() {
        let mut tb = ThrottleBuf::default();
        tb.push(b"ab");
        tb.push(b"cd");
        // 10ms < 50ms 窗口：不冲刷
        assert!(tb
            .take_due(Duration::from_millis(10), OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP)
            .is_none());
        // 60ms ≥ 窗口：合并为一个帧
        let out = tb
            .take_due(Duration::from_millis(60), OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP)
            .expect("到期应冲刷");
        assert_eq!(out, b"abcd");
        // 空缓冲永不冲刷
        assert!(tb
            .take_due(Duration::from_secs(10), OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP)
            .is_none());
        // 超硬上限：即使 elapsed=0 也立即冲刷（大输出防堆积）
        tb.push(&vec![b'x'; OUTPUT_FLUSH_CAP + 1]);
        assert!(tb
            .take_due(Duration::ZERO, OUTPUT_THROTTLE, OUTPUT_FLUSH_CAP)
            .is_some());
    }

    // —— 4. 路由声明：4 条端点全部 admin ——
    #[tokio::test]
    async fn routes_declare_admin_only_endpoints() {
        let h = TerminalRouteHandler::default();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 4, "应有 4 条路由: {routes:?}");
        for r in &routes {
            assert_eq!(r.handler_component, "terminal");
            // 鉴权红线：终端 = 最高权限面，全端点（含 node-snapshot）admin
            assert!(r.requires_auth, "终端全端点需 admin: {r:?}");
            assert_eq!(r.required_roles, vec!["admin".to_string()]);
        }
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Delete && r.path == "/api/v1/terminal/sessions/:id"));
        // node-snapshot 在路由表且 admin
        let snap = routes
            .iter()
            .find(|r| r.path == "/api/v1/terminal/node-snapshot")
            .expect("应声明 node-snapshot 路由");
        assert_eq!(snap.method, HttpMethod::Get);
        assert!(snap.requires_auth);
        assert_eq!(snap.required_roles, vec!["admin".to_string()]);
    }

    // —— 4b. node-snapshot 聚合形状：五字段齐全 + 百分比范围 + P2P 未注入为 null ——
    #[tokio::test]
    async fn node_snapshot_aggregates_shape() {
        let h = TerminalRouteHandler::default();
        let resp = h
            .handle(get_req("/api/v1/terminal/node-snapshot"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        // 形状：{version, uptime_secs, p2p_connected, disk_use_pct, mem_use_pct}
        let version = resp.body["version"].as_str().expect("version 应为字符串");
        assert!(!version.is_empty(), "version 非空（env 或包版本）");
        assert!(
            resp.body["uptime_secs"].is_u64(),
            "uptime_secs 应为非负整数: {resp:?}"
        );
        // 未注入 P2P Handle → null（前端渲染「未启用」）
        assert!(resp.body["p2p_connected"].is_null());
        for key in ["disk_use_pct", "mem_use_pct"] {
            let pct = resp.body[key]
                .as_f64()
                .unwrap_or_else(|| panic!("{key} 应为数值"));
            assert!((0.0..=100.0).contains(&pct), "{key} 应在 0-100: {pct}");
        }
        // 未知子路径 404（路由表不越界）
        let resp = h
            .handle(get_req("/api/v1/terminal/node-snapshot/extra"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // —— 4c. use_pct 纯函数：常规/边界（0 总量防除零/超界钳制/一位小数） ——
    #[test]
    fn use_pct_handles_zero_total_and_clamps() {
        fn close(a: f32, b: f32) -> bool {
            (a - b).abs() < 1e-3
        }
        assert_eq!(use_pct(0, 0), 0.0, "total=0 防除零 → 0");
        assert!(close(use_pct(50, 100), 50.0));
        assert!(
            close(use_pct(1, 3), 33.3),
            "一位小数四舍五入: {}",
            use_pct(1, 3)
        );
        assert!(close(use_pct(150, 100), 100.0), "超界钳制到 100");
        assert!(close(use_pct(0, 100), 0.0));
    }

    // —— 5. 参数校验：直连 key_path 非绝对路径 → 400 ——
    #[tokio::test]
    async fn ssh_key_path_must_be_absolute() {
        let h = TerminalRouteHandler::default();
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"ssh","host":"10.0.0.1","key_path":"~/.ssh/id_ed25519"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("绝对路径"));
        // 相对路径同理
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"ssh","host":"10.0.0.1","key_path":"keys/id"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // —— 6. 参数校验：ssh 缺 host → 400；kind 非法 → 400 ——
    #[tokio::test]
    async fn ssh_requires_host_and_known_kind() {
        let h = TerminalRouteHandler::default();
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"ssh"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("host"));

        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"telnet"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("local / ssh"));
    }

    // —— 7. target_id 解析：provisioning 注册表只读复用；未知 → 404 ——
    #[tokio::test]
    async fn ssh_unknown_target_id_returns_404() {
        let targets: Vec<SshTarget> = vec![];
        let h = TerminalRouteHandler::default().with_ssh_targets(Arc::new(move || targets.clone()));
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"ssh","target_id":"ghost"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "body: {resp:?}");
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("SSH 目标不存在"));
    }

    // ==================== 以下为真实 PTY 测试（#[cfg(unix)] 门） ====================
    // portable-pty 在 Linux 目标环境可用；非 unix（理论移植面）跳过。

    /// 测试会话清理守卫：测试结束（含断言失败展开）兜底 kill，防 bash 泄漏。
    /// kill 全同步（见 PtySession::kill），Drop 内无需 async。
    struct SessionGuard {
        sessions: Arc<TerminalSessions>,
        ids: Vec<String>,
    }
    impl SessionGuard {
        fn new(sessions: Arc<TerminalSessions>) -> Self {
            Self {
                sessions,
                ids: vec![],
            }
        }
        fn push(&mut self, id: String) {
            self.ids.push(id);
        }
    }
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            for id in std::mem::take(&mut self.ids) {
                self.sessions.kill_session(&id); // 幂等：已删的返回 false
            }
        }
    }

    // —— 8. 本地 PTY 往返：spawn bash → 写 echo → 读输出含执行结果 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn local_pty_spawn_and_echo_roundtrip() {
        let sessions = Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT));
        let mut guard = SessionGuard::new(sessions.clone());
        let info = sessions.spawn_local(80, 24).expect("本地会话创建失败");
        guard.push(info.session_id.clone());
        assert_eq!(info.kind, "local");
        assert_eq!(info.cols, 80);
        assert_eq!(info.rows, 24);

        let session = sessions.get(&info.session_id).unwrap();
        let mut rx = session.subscribe();
        // 命令输出与键入回显可区分：键入 os$((6*7))term，执行结果 os42term
        // 只会出现在真实执行后的输出里。
        session
            .write_input(b"echo os$((6*7))term\n")
            .expect("写入失败");
        let acc = collect_until_contains(&mut rx, "os42term", Duration::from_secs(10)).await;
        assert!(acc.contains("os42term"), "PTY 输出应含执行结果: {acc:?}");

        // resize 生效（PTY 尺寸更新 + info 快照跟随）
        session.resize(120, 40).expect("resize 失败");
        assert_eq!(session.info().cols, 120);
        assert_eq!(session.info().rows, 40);
    }

    // —— 9. 会话生命周期：创建 → 列表 → 删除 → 404 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn session_lifecycle_create_list_delete() {
        let sessions = Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT));
        let h = TerminalRouteHandler::with_sessions(sessions.clone());
        let mut guard = SessionGuard::new(sessions.clone());

        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"local","cols":100,"rows":30}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {resp:?}");
        let id = resp.body["session_id"].as_str().unwrap().to_string();
        assert!(id.starts_with("term-"));
        assert_eq!(resp.body["cols"], 100);
        guard.push(id.clone());

        let resp = h
            .handle(get_req("/api/v1/terminal/sessions"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["session_id"], id);
        assert_eq!(arr[0]["kind"], "local");

        let resp = h
            .handle(del_req(&format!("/api/v1/terminal/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(sessions.count(), 0);

        // 再删 → 404
        let resp = h
            .handle(del_req(&format!("/api/v1/terminal/sessions/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // —— 10. 会话上限：超限 429；删除释放配额后可再建 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn session_limit_returns_429() {
        let sessions = Arc::new(TerminalSessions::with_limits(2, IDLE_TIMEOUT));
        let h = TerminalRouteHandler::with_sessions(sessions.clone());
        let mut guard = SessionGuard::new(sessions.clone());

        for i in 0..2 {
            let resp = h
                .handle(post_req(
                    "/api/v1/terminal/sessions",
                    serde_json::json!({"kind":"local"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 201, "第 {i} 个会话应创建成功");
            guard.push(resp.body["session_id"].as_str().unwrap().to_string());
        }
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"local"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 429, "body: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("上限"));

        // 删一个后可再建（配额释放）
        let first = guard.ids[0].clone();
        sessions.kill_session(&first);
        let resp = h
            .handle(post_req(
                "/api/v1/terminal/sessions",
                serde_json::json!({"kind":"local"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "释放配额后应可再建");
    }

    // —— 11. EOF 清理：exit 命令 → exit 帧（真实退出码）+ 会话自移除 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn eof_cleanup_sends_exit_frame_and_removes_session() {
        let sessions = Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT));
        let mut guard = SessionGuard::new(sessions.clone());
        let info = sessions.spawn_local(80, 24).unwrap();
        guard.push(info.session_id.clone());
        let session = sessions.get(&info.session_id).unwrap();
        let mut rx = session.subscribe();

        session.write_input(b"exit\n").unwrap();

        // 等待 exit 帧（10s 上限）
        let mut exit_code: Option<i32> = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), rx.recv()).await
            {
                Ok(Ok(ServerFrame::Exit { code })) => {
                    exit_code = Some(code);
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => break,
            }
        }
        assert_eq!(exit_code, Some(0), "bash exit 应退出码 0");

        // 聚合任务自清理：注册表移除（轮询至多 2s——清理在 exit 帧后异步发生）
        let deadline = Instant::now() + Duration::from_secs(2);
        while sessions.get(&info.session_id).is_some() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            sessions.get(&info.session_id).is_none(),
            "EOF 后会话应自清理"
        );
    }

    // —— 12. 空闲回收（注入时钟）：拨回 last_active → reap → kill+清理 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn idle_reap_kills_inactive_sessions() {
        let sessions = Arc::new(TerminalSessions::with_limits(
            MAX_SESSIONS,
            Duration::from_secs(3600),
        ));
        let mut guard = SessionGuard::new(sessions.clone());
        let live = sessions.spawn_local(80, 24).unwrap();
        let idle = sessions.spawn_local(80, 24).unwrap();
        guard.push(live.session_id.clone());
        guard.push(idle.session_id.clone());

        // idle 会话拨回 2 小时前（> 1 小时阈值）；live 保持刚创建。
        sessions
            .get(&idle.session_id)
            .unwrap()
            .force_idle(Duration::from_secs(2 * 3600));

        let reaped = sessions.reap_idle();
        assert_eq!(reaped, 1, "只回收 1 个空闲会话");
        assert!(sessions.get(&idle.session_id).is_none(), "空闲会话应被清理");
        assert!(sessions.get(&live.session_id).is_some(), "活跃会话不受影响");
    }

    // ==================== PATH 注入：假 ssh 断言 argv ====================
    // （复用 provisioning 测试手法：全局锁串行化 + 临时目录假脚本；
    //   同步 runtime block_on 在锁内驱动，spawn 落在假 PATH 窗口。）

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_fake_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("PATH").unwrap_or_default();
        let combined = format!("{}:{}", dir.display(), old);
        std::env::set_var("PATH", &combined);
        let result = f();
        std::env::set_var("PATH", old);
        result
    }

    fn fake_bin(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("os-api-terminal-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // —— 13. ssh 会话 spawn：PATH 注入假 ssh 断言 argv（-tt/-p/-i/目标） ——
    // 注：#[test]（非 #[tokio::test]）——with_fake_path 锁内需自建 runtime
    // block_on（参考 provisioning 同款），避免「runtime 内建 runtime」panic。
    #[cfg(unix)]
    #[test]
    fn ssh_session_spawns_fake_ssh_with_expected_argv() {
        let dir = temp_dir("ssh-argv");
        let argv_file = dir.join("argv.txt");
        let argv_path = argv_file.display().to_string();
        let _ = std::fs::remove_file(&argv_file);
        // 假 ssh：把 argv 逐行落盘，然后挂住（保会话存活到测试清理 kill）。
        fake_bin(
            &dir,
            "ssh",
            &format!("for a in \"$@\"; do echo \"$a\" >> {argv_path}; done; sleep 30"),
        );

        let sessions = Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT));
        let mut guard = SessionGuard::new(sessions.clone());

        with_fake_path(&dir, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let h = TerminalRouteHandler::with_sessions(sessions.clone());
                let resp = h
                    .handle(post_req(
                        "/api/v1/terminal/sessions",
                        serde_json::json!({
                            "kind": "ssh",
                            "host": "10.7.7.7",
                            "port": 2222,
                            "user": "root",
                            "key_path": dir.join("id_test").display().to_string(),
                            "cols": 100,
                            "rows": 30,
                        }),
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 201, "body: {resp:?}");
                assert_eq!(resp.body["kind"], "ssh");
                assert_eq!(resp.body["target"], "root@10.7.7.7:2222");
                guard.push(resp.body["session_id"].as_str().unwrap().to_string());
            });
        });

        // 轮询 argv 文件落盘（spawn 后立即写）
        let deadline = Instant::now() + Duration::from_secs(5);
        let argv = loop {
            if argv_file.is_file() {
                if let Ok(content) = std::fs::read_to_string(&argv_file) {
                    if !content.is_empty() {
                        break content;
                    }
                }
            }
            assert!(Instant::now() < deadline, "假 ssh argv 未落盘");
            std::thread::sleep(Duration::from_millis(50));
        };
        let lines: Vec<&str> = argv.lines().collect();
        assert!(lines.contains(&"-tt"), "argv 应含 -tt: {lines:?}");
        assert!(lines.contains(&"-o"), "argv 应含 -o: {lines:?}");
        assert!(
            lines.contains(&"StrictHostKeyChecking=accept-new"),
            "argv 应含 accept-new: {lines:?}"
        );
        let p_pos = lines.iter().position(|a| *a == "-p").expect("应有 -p");
        assert_eq!(lines[p_pos + 1], "2222");
        let i_pos = lines.iter().position(|a| *a == "-i").expect("应有 -i");
        assert_eq!(lines[i_pos + 1], dir.join("id_test").display().to_string());
        assert_eq!(
            lines.last().copied(),
            Some("root@10.7.7.7"),
            "目标在最后: {lines:?}"
        );
        // 无 shell 拼接的旁证：恰好 8 个参数（-tt/-o/值/-p/值/-i/值/目标）
        assert_eq!(lines.len(), 8, "argv 直传无拼接: {lines:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 14. DELETE kill：会话删除后子进程连带终止 ——
    #[cfg(unix)]
    #[tokio::test]
    async fn delete_session_kills_child_process() {
        let sessions = Arc::new(TerminalSessions::with_limits(MAX_SESSIONS, IDLE_TIMEOUT));
        let mut guard = SessionGuard::new(sessions.clone());
        let info = sessions.spawn_local(80, 24).unwrap();
        guard.push(info.session_id.clone());
        let session = sessions.get(&info.session_id).unwrap();
        // 打一个长睡眠子进程（bash 的子进程），验证会话 kill 连带清理。
        session.write_input(b"sleep 300\n").unwrap();

        // 等 sleep 子进程出现（扫 /proc 找 bash 的子进程）
        let deadline = Instant::now() + Duration::from_secs(5);
        let child_pid = loop {
            if let Some(pid) = child_pid_of_session(&session) {
                break pid;
            }
            assert!(Instant::now() < deadline, "sleep 子进程未出现");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        sessions.kill_session(&info.session_id);
        assert!(sessions.get(&info.session_id).is_none());

        // 子进程应被终止（HUP 传播；轮询最多 3s）
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut alive = true;
        while Instant::now() < deadline {
            alive = std::path::Path::new(&format!("/proc/{child_pid}")).exists();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!alive, "kill 后子进程应终止（pid={child_pid}）");
    }

    /// 经 /proc 找会话 bash 的子进程 pid（sleep）——验证进程组连带清理。
    fn child_pid_of_session(session: &Arc<PtySession>) -> Option<i32> {
        // try_lock（异步上下文禁 blocking_lock）；child 锁只在 EOF wait 时长持。
        let bash_pid = {
            let child = session.child.try_lock().ok()?;
            child.process_id()? as i32
        };
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            if pid == bash_pid {
                continue;
            }
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // stat 第 4 字段 ppid（在 comm（可能含空格括号）之后）
                if let Some(close) = stat.rfind(')') {
                    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                    if rest.len() >= 2 {
                        if let Ok(ppid) = rest[1].parse::<i32>() {
                            if ppid == bash_pid {
                                return Some(pid);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}
