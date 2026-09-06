//! 工具链手动安装（node/uv/cargo）—— `agenthub.rs` 的子模块。
//!
//! 定位：AgentHub 前端检测到「缺少 node/npm / uv / cargo 工具链」时按钮禁用且
//! 无处可装——本模块补上**手动触发**的用户态安装器（不自动装、不用 sudo/apt：
//! os-api 进程无 root，一律装到用户目录）。
//!
//! # 安装源矩阵（中国镜像优先，全部可 env 覆盖）
//!
//! | 工具链 | 安装方式 | 主源（优先） | 回退源 | 落点 |
//! |--------|----------|--------------|--------|------|
//! | `node`（覆盖 node+npm）| nvm 安装脚本 `curl -o- <url> \| bash`，再 `. nvm.sh && nvm install --lts` | ghfast.top 镜像（`https://ghfast.top/https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh`，且 `METHOD=script` + `NVM_SOURCE` 让 nvm.sh 主体也走镜像）；node 二进制 `NVM_NODEJS_ORG_MIRROR=https://npmmirror.com/mirrors/node` | 官方 `raw.githubusercontent.com` 原链重试一次（无镜像 env）| `~/.nvm`（版本目录 `~/.nvm/versions/node/<ver>/bin`），完成后幂等追加 `source nvm.sh` 到 `~/.bashrc` |
//! | `uv` | 官方安装脚本 `curl -LsSf https://astral.sh/uv/install.sh \| sh` | astral.sh 直连 | 同一脚本 + `INSTALLER_DOWNLOAD_URL=ghfast.top 代理 GitHub Releases`（uv 官方支持的下载源覆盖变量）| `~/.local/bin/uv` |
//! | `cargo` | rustup 安装脚本 `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh -s -- -y --profile minimal --default-toolchain stable` | 清华 TUNA 镜像（`RUSTUP_DIST_SERVER` / `RUSTUP_UPDATE_ROOT` 指向 `mirrors.tuna.tsinghua.edu.cn/rustup`，脚本本身 sh.rustup.rs 直连）| 官方 static.rust-lang.org（去掉镜像 env 重试一次）| `~/.cargo/bin/cargo` |
//!
//! curl/bash 为系统基础件，不提供安装。幂等：`~/.nvm/nvm.sh` 已存在则跳过 nvm
//! 安装直接 `nvm install`；重复安装同工具链（有 running 任务）409；探测已命中
//! → 任务直接 done 提示已安装。
//!
//! # 异步任务（llm_envs 同款）
//!
//! 安装要下几十 MB（rustup 200MB+），`POST` 立即返回 `202 {task_id}`，后台
//! std 线程执行；任务态存进程内 `Mutex<HashMap<String, ToolchainTask>>`（环形
//! 日志上限 200 行），前端轮询 `GET /agenthub/toolchain/install/tasks/:id` 看
//! 进度。服务重启任务态即清（安装物在磁盘上，重开页面探测自会命中）。
//!
//! # 执行器抽象（真实数据铁律）
//!
//! 全部外部命令经 [`ToolchainExecutor`] 抽象：生产 [`ProcessExecutor`]
//! （std::process 真实执行 + 30min 超时 kill），测试注入 mock（cfg(test) 内
//! 定义，绝不真跑 curl/网络）；探测函数同样可注入（隔离宿主 PATH）。
//!
//! # 探测口径（与 agenthub.rs 同步）
//!
//! 安装物都在用户目录（`~/.nvm/...` / `~/.local/bin` / `~/.cargo/bin`），非
//! 登录 shell 的 os-api 进程 PATH 看不到——**探测/spawn 一律经
//! `agenthub::resolve_bin_in` 解析已知安装位置的完整路径**（前端按钮解禁后，
//! agent 安装任务即用该完整路径调用 npm/uv/cargo，不依赖进程 PATH 注入）。
//!
//! # env 清单（全部 `NEXOS_AGENTHUB_` 前缀；详见 docs/AGENT_HUB.md）
//!
//! - `NEXOS_AGENTHUB_HOME`：安装/探测根目录（缺省 `$HOME` → `/home/$USER` → `/root`）
//! - `NEXOS_AGENTHUB_NVM_INSTALL_URL`：nvm 安装脚本 URL 覆盖（设置后不再叠加镜像/回退链）
//! - `NEXOS_AGENTHUB_NVM_NODE_MIRROR`：node 二进制镜像（缺省 npmmirror.com）
//! - `NEXOS_AGENTHUB_UV_INSTALL_URL`：uv 安装脚本 URL 覆盖
//! - `NEXOS_AGENTHUB_RUSTUP_INSTALL_URL`：rustup 安装脚本 URL 覆盖
//!
//! 日志：进程内一律 `eprintln!`（`[agenthub]` 前缀），不用 tracing。

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use super::agenthub::resolve_bin_in;
use crate::error::ApiGatewayError;
use crate::gateway::{ApiResponse, HttpMethod, RouteSpec};

// ----------------------------------------------------------------------------
// 常量：安装源矩阵与 env
// ----------------------------------------------------------------------------

/// nvm 版本（安装脚本与 nvm.sh 主体同版本）。
pub const NVM_VERSION: &str = "v0.40.1";

/// nvm 官方安装脚本（回退源；主源为其 ghfast.top 镜像）。
pub const NVM_INSTALL_URL_OFFICIAL: &str =
    "https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh";

/// ghfast.top 镜像前缀（GitHub raw/releases 代理，foundryup 先例）。
pub const GHFAST_PREFIX: &str = "https://ghfast.top/";

/// node 二进制镜像缺省（npmmirror）。
pub const NVM_NODE_MIRROR_DEFAULT: &str = "https://npmmirror.com/mirrors/node";

/// uv 官方安装脚本。
pub const UV_INSTALL_URL_DEFAULT: &str = "https://astral.sh/uv/install.sh";

/// uv release 资产官方基址（回退时经 ghfast 代理）。
pub const UV_RELEASE_BASE_OFFICIAL: &str =
    "https://github.com/astral-sh/uv/releases/latest/download";

/// rustup 官方安装脚本（脚本本体；dist 走 RUSTUP_DIST_SERVER 镜像）。
pub const RUSTUP_INSTALL_URL_DEFAULT: &str = "https://sh.rustup.rs";

/// rustup dist 清华镜像（rustc/cargo/toolchain 下载源）。
pub const RUSTUP_DIST_MIRROR: &str = "https://mirrors.tuna.tsinghua.edu.cn/rustup";

/// rustup 自身更新根清华镜像（安装器取 rustup-init 的源）。
pub const RUSTUP_UPDATE_MIRROR: &str = "https://mirrors.tuna.tsinghua.edu.cn/rustup/rustup";

/// 探测/安装根目录覆盖 env（测试与运维诊断用）。
pub const ENV_HOME: &str = "NEXOS_AGENTHUB_HOME";

/// nvm 安装脚本 URL 覆盖 env。
const ENV_NVM_INSTALL_URL: &str = "NEXOS_AGENTHUB_NVM_INSTALL_URL";

/// node 二进制镜像覆盖 env。
const ENV_NVM_NODE_MIRROR: &str = "NEXOS_AGENTHUB_NVM_NODE_MIRROR";

/// uv 安装脚本 URL 覆盖 env。
const ENV_UV_INSTALL_URL: &str = "NEXOS_AGENTHUB_UV_INSTALL_URL";

/// rustup 安装脚本 URL 覆盖 env。
const ENV_RUSTUP_INSTALL_URL: &str = "NEXOS_AGENTHUB_RUSTUP_INSTALL_URL";

/// 可安装工具链名（node 覆盖 node+npm 两者；curl/bash 系统基础件不装）。
pub const INSTALLABLE_TOOLCHAINS: [&str; 3] = ["node", "uv", "cargo"];

/// 任务日志环形上限（行）。
const TASK_LOG_MAX_LINES: usize = 200;

/// 执行器返回的 stdout+stderr 截尾上限（字符）。
const OUTPUT_TAIL_CHARS: usize = 4000;

/// 单条安装命令超时（nvm/rustup 下载数十 MB，30min 兜底防挂死）。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 工具链可用性探测签名（home + 工具链名 → 命中路径）。
type ProbeFn = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// ghfast 镜像 URL 拼接（前缀 + 原始 GitHub URL）。
fn ghfast(url: &str) -> String {
    format!("{GHFAST_PREFIX}{url}")
}

/// 读非空 env。
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 安装/探测根目录：env 覆盖 → `$HOME` → `/home/$USER` → `/root`（systemd
/// 系统服务可能不带 HOME，不写死具体用户名）。
pub fn toolchain_home() -> String {
    if let Some(h) = env_non_empty(ENV_HOME) {
        return h;
    }
    if let Some(h) = env_non_empty("HOME") {
        return h;
    }
    if let Some(u) = env_non_empty("USER") {
        if u != "root" {
            return format!("/home/{u}");
        }
    }
    "/root".to_string()
}

/// 当前 Unix epoch 秒。
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 字符串截尾（保留最后 n 个字符）。
fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().skip(s.chars().count() - n).collect()
}

/// 路径是否可执行文件（Unix exec 位；安装完成校验用）。
fn is_executable_file(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// ToolchainExecutor 抽象（生产真实执行 / 测试注入 mock，llm_envs 同款）
// ----------------------------------------------------------------------------

/// 外部命令执行器抽象：生产 [`ProcessExecutor`] 真跑子进程，测试注入 mock。
///
/// 返回 `(退出码, stdout+stderr 截尾)`；命令无法启动/超时返回 Err（带原因）。
pub trait ToolchainExecutor: Send + Sync {
    /// 执行 `argv[0] argv[1..]`，注入 `env_kv` 环境变量（在进程当前 env 之上）。
    fn run(&self, argv: &[&str], env_kv: &[(String, String)]) -> Result<(i32, String), String>;
}

/// 生产执行器：std::process 真实执行，30min 超时 kill（安装器可能慢网络）。
struct ProcessExecutor;

impl ToolchainExecutor for ProcessExecutor {
    fn run(&self, argv: &[&str], env_kv: &[(String, String)]) -> Result<(i32, String), String> {
        if argv.is_empty() {
            return Err("空命令".to_string());
        }
        let mut cmd = Command::new(argv[0]);
        cmd.args(&argv[1..]);
        for (k, v) in env_kv {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| format!("启动 {} 失败: {e}", argv[0]))?;
        let pid = child.id();
        // wait_with_output 是阻塞调用：搬到子线程 + channel 限时，超时 kill 兜底。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(INSTALL_TIMEOUT) {
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let code = out.status.code().unwrap_or(-1);
                Ok((code, tail_chars(&text, OUTPUT_TAIL_CHARS)))
            }
            Ok(Err(e)) => Err(format!("等待 {} 完成（pid {pid}）失败: {e}", argv[0])),
            Err(_) => {
                eprintln!(
                    "[agenthub] 工具链安装命令超时（>{INSTALL_TIMEOUT:?}），kill pid {pid}: {}",
                    argv.join(" ")
                );
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                // 等 waiter 线程收尸（kill 后管道关闭，wait_with_output 很快返回）
                let _ = rx.recv_timeout(Duration::from_secs(10));
                Err(format!(
                    "命令超时（>{INSTALL_TIMEOUT:?}）已 kill: {}",
                    argv.join(" ")
                ))
            }
        }
    }
}

// ----------------------------------------------------------------------------
// 探测（已知安装位置兜底 → PATH；与 agenthub.rs 的 resolve_bin_in 同口径）
// ----------------------------------------------------------------------------

/// 解析工具链可执行文件完整路径：用户级安装位置（`~/.local/bin`、`~/.cargo/bin`、
/// `~/.nvm/versions/node/*/bin`，经 [`resolve_bin_in`]，取最高 node 版本）命中即
/// 返回；否则 `command -v` 查 PATH；都没有返回 None。
fn toolchain_bin_path(home: &str, name: &str) -> Option<String> {
    let resolved = resolve_bin_in(home, name);
    if resolved != name {
        return Some(resolved);
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if first.is_empty() {
        None
    } else {
        Some(first)
    }
}

/// 工具链是否已可用（node 要求 node+npm 两者都在——安装覆盖两者）。
fn toolchain_available(home: &str, name: &str) -> Option<String> {
    match name {
        "node" => {
            let node = toolchain_bin_path(home, "node")?;
            toolchain_bin_path(home, "npm").map(|_| node)
        }
        "uv" | "cargo" => toolchain_bin_path(home, name),
        _ => None,
    }
}

/// 仅探测用户目录安装位置（[`resolve_bin_in`]，无 PATH 兜底）——测试注入用，
/// 隔离宿主 PATH（安装产物落临时 HOME 的用例靠它确定性命中/未命中）。
/// "node" 要求 node+npm 两者（安装覆盖两者）；其余名字（npm/uv/cargo…）单探测。
fn toolchain_available_user_dirs(home: &str, name: &str) -> Option<String> {
    if name == "node" {
        let node = resolve_bin_in(home, "node");
        if node == "node" {
            return None;
        }
        if resolve_bin_in(home, "npm") == "npm" {
            return None;
        }
        return Some(node);
    }
    let p = resolve_bin_in(home, name);
    if p == name {
        None
    } else {
        Some(p)
    }
}

// ----------------------------------------------------------------------------
// DTO 与任务态
// ----------------------------------------------------------------------------

/// 工具链安装任务（进程内态；重启即清，安装物在磁盘上）。
#[derive(Debug, Clone)]
pub struct ToolchainTask {
    pub id: String,
    /// `node` / `uv` / `cargo`。
    pub toolchain: String,
    /// `running` / `done` / `error`。
    pub status: String,
    /// 环形日志（上限 [`TASK_LOG_MAX_LINES`] 行）。
    pub log: Vec<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// 任务详情视图（GET tasks/:id 响应）。
#[derive(Debug, Serialize)]
struct TaskView {
    id: String,
    toolchain: String,
    status: String,
    log: Vec<String>,
    started_at: i64,
    finished_at: Option<i64>,
}

impl From<&ToolchainTask> for TaskView {
    fn from(t: &ToolchainTask) -> Self {
        Self {
            id: t.id.clone(),
            toolchain: t.toolchain.clone(),
            status: t.status.clone(),
            log: t.log.clone(),
            started_at: t.started_at,
            finished_at: t.finished_at,
        }
    }
}

/// 工具链安装子模块状态（嵌入 [`super::agenthub::AgentHubRouteHandler`]）。
pub struct ToolchainState {
    /// 进程内任务注册表（id → 任务）。
    tasks: Arc<Mutex<HashMap<String, ToolchainTask>>>,
    /// 任务 id 计数器。
    task_seq: AtomicU64,
    /// 命令执行器（生产 ProcessExecutor；测试注入 mock）。
    executor: Arc<dyn ToolchainExecutor>,
    /// 可用性探测（生产 [`toolchain_available`]；测试可注入恒 None 隔离宿主 PATH）。
    probe: ProbeFn,
    /// 安装/探测根目录。
    home: String,
}

impl ToolchainState {
    /// 生产构造：ProcessExecutor + 真实探测 + env 解析的根目录。
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            task_seq: AtomicU64::new(0),
            executor: Arc::new(ProcessExecutor),
            probe: Arc::new(toolchain_available),
            home: toolchain_home(),
        }
    }

    /// 测试注入构造：mock 执行器 + 固定根目录 + 仅用户目录探测（隔离宿主
    /// PATH——安装产物落临时 HOME 的用例确定性命中，不依赖宿主装没装 node）。
    #[must_use]
    pub fn with_executor(home: String, executor: Arc<dyn ToolchainExecutor>) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            task_seq: AtomicU64::new(0),
            executor,
            probe: Arc::new(toolchain_available_user_dirs),
            home,
        }
    }

    /// 根目录（任务线程用）。
    #[must_use]
    pub fn home(&self) -> &str {
        &self.home
    }

    /// 该工具链是否有 running 任务（重复安装 409 守卫）。
    fn has_running(&self, toolchain: &str) -> bool {
        self.tasks
            .lock()
            .map(|m| {
                m.values()
                    .any(|t| t.toolchain == toolchain && t.status == "running")
            })
            .unwrap_or(false)
    }

    /// 登记任务（running 态）并返回 id。
    fn register_task(&self, toolchain: &str) -> String {
        let task_id = format!(
            "tctask-{}",
            self.task_seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        self.tasks
            .lock()
            .expect("tctasks poisoned")
            .insert(
                task_id.clone(),
                ToolchainTask {
                    id: task_id.clone(),
                    toolchain: toolchain.to_string(),
                    status: "running".into(),
                    log: Vec::new(),
                    started_at: now_epoch(),
                    finished_at: None,
                },
            );
        task_id
    }

    /// 登记一个立即完成的任务（幂等命中「已安装」，不 spawn 线程）。
    fn register_done(&self, toolchain: &str, line: &str) -> String {
        let task_id = self.register_task(toolchain);
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(t) = tasks.get_mut(&task_id) {
                t.log.push(line.to_string());
                t.status = "done".into();
                t.finished_at = Some(now_epoch());
            }
        }
        task_id
    }

    /// 启动安装后台线程（fire-and-forget；收尾统一 done/error）。
    fn spawn_install(&self, toolchain: &str) -> String {
        let task_id = self.register_task(toolchain);
        let ctx = TaskCtx {
            task_id: task_id.clone(),
            toolchain: toolchain.to_string(),
            home: self.home.clone(),
            executor: Arc::clone(&self.executor),
            probe: Arc::clone(&self.probe),
            tasks: Arc::clone(&self.tasks),
        };
        let job: fn(&TaskCtx) -> Result<(), String> = match toolchain {
            "uv" => install_uv,
            "cargo" => install_cargo,
            _ => install_node,
        };
        std::thread::spawn(move || match job(&ctx) {
            Ok(()) => task_finish(&ctx, "done", "✔ 安装完成"),
            Err(e) => {
                eprintln!(
                    "[agenthub] 工具链安装任务失败：{}（{}）：{e}",
                    ctx.task_id, ctx.toolchain
                );
                task_finish(&ctx, "error", &format!("✘ 失败：{e}"));
            }
        });
        task_id
    }
}

impl Default for ToolchainState {
    fn default() -> Self {
        Self::new()
    }
}

/// 任务线程共享态。
struct TaskCtx {
    task_id: String,
    toolchain: String,
    home: String,
    executor: Arc<dyn ToolchainExecutor>,
    probe: ProbeFn,
    tasks: Arc<Mutex<HashMap<String, ToolchainTask>>>,
}

/// 任务日志追加一行（环形上限 [`TASK_LOG_MAX_LINES`]）。
fn task_log(ctx: &TaskCtx, line: &str) {
    for l in line.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(mut tasks) = ctx.tasks.lock() {
            if let Some(t) = tasks.get_mut(&ctx.task_id) {
                t.log.push(l.to_string());
                if t.log.len() > TASK_LOG_MAX_LINES {
                    let cut = t.log.len() - TASK_LOG_MAX_LINES;
                    t.log.drain(0..cut);
                }
            }
        }
    }
}

/// 任务收尾（状态 + finished_at + 收尾日志行）。
fn task_finish(ctx: &TaskCtx, status: &str, line: &str) {
    task_log(ctx, line);
    if let Ok(mut tasks) = ctx.tasks.lock() {
        if let Some(t) = tasks.get_mut(&ctx.task_id) {
            t.status = status.to_string();
            t.finished_at = Some(now_epoch());
        }
    }
    eprintln!(
        "[agenthub] 工具链安装任务{}：{}（{}）",
        if status == "done" { "完成" } else { "失败" },
        ctx.task_id,
        ctx.toolchain
    );
}

/// 经执行器跑一条命令：命令行 + env + 输出截尾进任务日志；非 0 退出码返回 Err。
fn run_logged(
    ctx: &TaskCtx,
    argv: &[&str],
    env_kv: &[(String, String)],
) -> Result<String, String> {
    task_log(ctx, &format!("$ {}", argv.join(" ")));
    for (k, v) in env_kv {
        task_log(ctx, &format!("  env {k}={v}"));
    }
    match ctx.executor.run(argv, env_kv) {
        Ok((code, out)) => {
            if !out.trim().is_empty() {
                task_log(ctx, &out);
            }
            if code == 0 {
                Ok(out)
            } else {
                Err(format!("命令退出码 {code}：{}", argv.join(" ")))
            }
        }
        Err(e) => {
            task_log(ctx, &format!("执行失败：{e}"));
            Err(e)
        }
    }
}

/// 跑 `sh -c "curl … | sh/bash …"` 安装脚本（管道形式；URL 均为常量/env 派生，
/// 无用户可控拼接面）。
fn run_installer_script(
    ctx: &TaskCtx,
    curl_expr: &str,
    env_kv: &[(String, String)],
) -> Result<String, String> {
    run_logged(ctx, &["sh", "-c", curl_expr], env_kv)
}

// ----------------------------------------------------------------------------
// 安装器（node / uv / cargo）
// ----------------------------------------------------------------------------

/// 幂等把 nvm 初始化追加进 `~/.bashrc`（已含 nvm 字样——含 nvm 安装脚本自己
/// 追加的行——则不动）。
fn ensure_nvm_in_bashrc(home: &str) -> Result<(), String> {
    let bashrc = Path::new(home).join(".bashrc");
    let existing = std::fs::read_to_string(&bashrc).unwrap_or_default();
    if existing.contains("nvm.sh") {
        return Ok(());
    }
    let snippet = "\n# NexOS AgentHub: nvm 工具链（幂等追加）\nexport NVM_DIR=\"$HOME/.nvm\"\n[ -s \"$NVM_DIR/nvm.sh\" ] && \\. \"$NVM_DIR/nvm.sh\"\n";
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&bashrc)
        .map_err(|e| format!("打开 {} 失败: {e}", bashrc.display()))?;
    f.write_all(snippet.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {e}", bashrc.display()))
}

/// node+npm 安装（nvm）：脚本装 nvm 到 ~/.nvm（镜像优先/官方回退）→
/// `nvm install --lts`（node 二进制走 npmmirror）→ 校验 → .bashrc 幂等追加。
fn install_node(ctx: &TaskCtx) -> Result<(), String> {
    let nvm_dir = format!("{}/.nvm", ctx.home);
    let nvm_sh = format!("{nvm_dir}/nvm.sh");
    if Path::new(&nvm_sh).exists() {
        task_log(ctx, "~/.nvm/nvm.sh 已存在，跳过 nvm 安装（幂等），直接 nvm install");
    } else {
        let override_url = env_non_empty(ENV_NVM_INSTALL_URL);
        if let Some(url) = override_url {
            // 运维显式覆盖：单次尝试，不叠加镜像/回退链
            task_log(ctx, &format!("使用 {ENV_NVM_INSTALL_URL} 指定的 nvm 安装脚本"));
            run_installer_script(ctx, &format!("curl -o- {url} | bash"), &[])?;
        } else {
            // 主：ghfast.top 镜像（METHOD=script 强制 curl 脚本安装避开 github.com
            // git clone；NVM_SOURCE 让 nvm.sh 主体同样走镜像）
            let mirror_kv: Vec<(String, String)> = vec![
                ("METHOD".into(), "script".into()),
                (
                    "NVM_SOURCE".into(),
                    ghfast(&format!(
                        "https://raw.githubusercontent.com/nvm-sh/nvm/{NVM_VERSION}/nvm.sh"
                    )),
                ),
            ];
            let first = run_installer_script(
                ctx,
                &format!("curl -o- {} | bash", ghfast(NVM_INSTALL_URL_OFFICIAL)),
                &mirror_kv,
            );
            if let Err(e1) = first {
                task_log(ctx, &format!("镜像尝试失败（{e1}），回退官方源重试一次"));
                run_installer_script(
                    ctx,
                    &format!("curl -o- {NVM_INSTALL_URL_OFFICIAL} | bash"),
                    &[],
                )
                .map_err(|e2| format!("镜像（{e1}）与官方源（{e2}）均失败"))?;
            }
        }
    }
    // nvm install --lts（node+npm 同装；二进制走 npmmirror 镜像）
    let node_mirror =
        env_non_empty(ENV_NVM_NODE_MIRROR).unwrap_or_else(|| NVM_NODE_MIRROR_DEFAULT.to_string());
    run_logged(
        ctx,
        &["bash", "-c", &format!(". '{nvm_sh}' && nvm install --lts")],
        &[
            ("NVM_DIR".into(), nvm_dir),
            ("NVM_NODEJS_ORG_MIRROR".into(), node_mirror),
        ],
    )?;
    let node = (ctx.probe)(&ctx.home, "node")
        .ok_or("nvm install 完成后仍未找到 node（~/.nvm/versions/node/*/bin/node）")?;
    (ctx.probe)(&ctx.home, "npm")
        .ok_or("nvm install 完成后仍未找到 npm（~/.nvm/versions/node/*/bin/npm）")?;
    task_log(ctx, &format!("node 就绪：{node}"));
    ensure_nvm_in_bashrc(&ctx.home)?;
    task_log(ctx, "已确保 ~/.bashrc 含 nvm 初始化（幂等）");
    Ok(())
}

/// uv 安装：官方脚本直连；失败回退同一脚本 + `INSTALLER_DOWNLOAD_URL` 走
/// ghfast 代理 GitHub Releases（uv 官方支持的下载源覆盖变量）。
fn install_uv(ctx: &TaskCtx) -> Result<(), String> {
    let url =
        env_non_empty(ENV_UV_INSTALL_URL).unwrap_or_else(|| UV_INSTALL_URL_DEFAULT.to_string());
    let first = run_installer_script(ctx, &format!("curl -LsSf {url} | sh"), &[]);
    if let Err(e1) = first {
        if env_non_empty(ENV_UV_INSTALL_URL).is_some() {
            return Err(e1);
        }
        task_log(ctx, &format!("官方源尝试失败（{e1}），回退 ghfast 镜像重试一次"));
        run_installer_script(
            ctx,
            &format!("curl -LsSf {url} | sh"),
            &[(
                "INSTALLER_DOWNLOAD_URL".into(),
                ghfast(UV_RELEASE_BASE_OFFICIAL),
            )],
        )
        .map_err(|e2| format!("官方源（{e1}）与 ghfast 镜像（{e2}）均失败"))?;
    }
    let uv = format!("{}/.local/bin/uv", ctx.home);
    if !is_executable_file(&uv) {
        return Err(format!("安装完成后 {uv} 不存在或不可执行"));
    }
    task_log(ctx, &format!("uv 就绪：{uv}"));
    Ok(())
}

/// cargo 安装（rustup minimal profile）：清华 TUNA dist 镜像优先，失败回退
/// 官方源（去镜像 env）重试一次。
fn install_cargo(ctx: &TaskCtx) -> Result<(), String> {
    let url = env_non_empty(ENV_RUSTUP_INSTALL_URL)
        .unwrap_or_else(|| RUSTUP_INSTALL_URL_DEFAULT.to_string());
    let script = format!(
        "curl --proto '=https' --tlsv1.2 -sSf {url} | sh -s -- -y --profile minimal --default-toolchain stable"
    );
    let use_mirror = env_non_empty(ENV_RUSTUP_INSTALL_URL).is_none();
    let first = if use_mirror {
        run_installer_script(
            ctx,
            &script,
            &[
                ("RUSTUP_DIST_SERVER".into(), RUSTUP_DIST_MIRROR.into()),
                ("RUSTUP_UPDATE_ROOT".into(), RUSTUP_UPDATE_MIRROR.into()),
            ],
        )
    } else {
        run_installer_script(ctx, &script, &[])
    };
    if let Err(e1) = first {
        if !use_mirror {
            return Err(e1);
        }
        task_log(ctx, &format!("清华镜像尝试失败（{e1}），回退官方源重试一次"));
        run_installer_script(ctx, &script, &[])
            .map_err(|e2| format!("清华镜像（{e1}）与官方源（{e2}）均失败"))?;
    }
    let cargo = format!("{}/.cargo/bin/cargo", ctx.home);
    if !is_executable_file(&cargo) {
        return Err(format!("安装完成后 {cargo} 不存在或不可执行"));
    }
    task_log(ctx, &format!("cargo 就绪：{cargo}"));
    Ok(())
}

// ----------------------------------------------------------------------------
// REST 端点（路由 specs + 处理；由 agenthub.rs 的 routes()/handle() 挂载）
// ----------------------------------------------------------------------------

/// 工具链安装路由 specs（handler_component=agenthub；写 admin，读公开——
/// 与既有 agenthub 任务端点同风格）。
#[must_use]
pub fn route_specs() -> Vec<RouteSpec> {
    fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: "agenthub".to_string(),
            requires_auth,
            required_roles: if requires_auth {
                vec!["admin".into()]
            } else {
                vec![]
            },
        }
    }
    vec![
        spec(HttpMethod::Post, "/api/v1/agenthub/toolchain/install", true),
        spec(
            HttpMethod::Get,
            "/api/v1/agenthub/toolchain/install/tasks/:id",
            false,
        ),
    ]
}

/// `POST /api/v1/agenthub/toolchain/install` 请求体。
#[derive(Debug, serde::Deserialize)]
struct InstallBody {
    name: String,
}

/// 处理 `/api/v1/agenthub/toolchain*`（`segs` 为 `toolchain` 之后的段）。
pub async fn handle(
    state: &ToolchainState,
    method: HttpMethod,
    segs: &[&str],
    body: serde_json::Value,
) -> Result<ApiResponse, ApiGatewayError> {
    match (method, segs) {
        // —— POST /toolchain/install —— 手动安装 → 后台任务 → 202 {task_id}
        (HttpMethod::Post, ["install"]) => {
            let body: InstallBody = serde_json::from_value(body).map_err(|e| {
                ApiGatewayError::Internal(format!("解析工具链安装请求体失败: {e}"))
            })?;
            let name = body.name.trim().to_string();
            if !INSTALLABLE_TOOLCHAINS.contains(&name.as_str()) {
                return Ok(error_response(
                    400,
                    &format!(
                        "name 必须是 {INSTALLABLE_TOOLCHAINS:?} 之一（node 覆盖 node+npm）"
                    ),
                ));
            }
            if state.has_running(&name) {
                return Ok(error_response(
                    409,
                    &format!("工具链 {name} 已有正在执行的安装任务"),
                ));
            }
            // 幂等：探测命中（含 ~/.nvm 等用户目录兜底）→ 任务直接 done
            let probe = Arc::clone(&state.probe);
            let home = state.home().to_string();
            let probe_name = name.clone();
            let available = tokio::task::spawn_blocking(move || probe(&home, &probe_name))
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("工具链探测 join 失败: {e}")))?;
            let (task_id, status) = match available {
                Some(path) => {
                    eprintln!("[agenthub] 工具链 {name} 已安装（探测命中 {path}），幂等返回");
                    let id = state.register_done(
                        &name,
                        &format!("已安装（探测命中 {path}），无需重复安装"),
                    );
                    (id, "done".to_string())
                }
                None => {
                    eprintln!("[agenthub] 开始工具链安装：{name}（用户态，任务后台执行）");
                    (state.spawn_install(&name), "running".to_string())
                }
            };
            Ok(ApiResponse {
                status: 202,
                body: serde_json::json!({
                    "task_id": task_id,
                    "toolchain": name,
                    "status": status,
                }),
                headers: serde_json::json!({}),
            })
        }

        // —— GET /toolchain/install/tasks/:id —— 单任务（含环形日志）
        (HttpMethod::Get, ["install", "tasks", id]) => {
            let found = state
                .tasks
                .lock()
                .ok()
                .and_then(|m| m.get(*id).map(TaskView::from));
            match found {
                Some(v) => Ok(ApiResponse {
                    status: 200,
                    body: serde_json::to_value(v).map_err(|e| {
                        ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
                    })?,
                    headers: serde_json::json!({}),
                }),
                None => Ok(error_response(404, &format!("工具链安装任务不存在: {id}"))),
            }
        }

        _ => Ok(error_response(404, "agenthub-toolchain: 未匹配的路由")),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

// ----------------------------------------------------------------------------
// 单元测试（mock 执行器驱动；绝不真跑 curl / 网络 / 写真实 HOME）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- mock 执行器（仅测试；生产恒 ProcessExecutor）----

    /// 恒成功执行器：返回 (0, "ok")——不落产物（用于失败路径校验）。
    struct AlwaysOk;

    impl ToolchainExecutor for AlwaysOk {
        fn run(
            &self,
            _argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            Ok((0, "ok".into()))
        }
    }

    /// 模拟安装器：看到触发子串时真实落可执行文件（伪装安装产物），其余命令
    /// 恒成功；可选 fail_trigger 命中即退出码 1（驱动回退路径）。让「安装后
    /// 校验文件存在 + 探测命中临时 HOME」的全链路可测且不联网。
    struct SimInstall {
        trigger: &'static str,
        /// 产物绝对路径（测试构造时拼好临时 HOME）。
        artifacts: Vec<String>,
        fail_trigger: Option<&'static str>,
    }

    impl ToolchainExecutor for SimInstall {
        fn run(
            &self,
            argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            let joined = argv.join(" ");
            if let Some(f) = self.fail_trigger {
                if joined.contains(f) {
                    return Ok((1, format!("boom: {f}")));
                }
            }
            if joined.contains(self.trigger) {
                for a in &self.artifacts {
                    let p = std::path::PathBuf::from(a);
                    if let Some(dir) = p.parent() {
                        std::fs::create_dir_all(dir).expect("创建产物目录");
                    }
                    std::fs::write(&p, b"#!/bin/sh\n").expect("写产物");
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &p,
                        std::fs::Permissions::from_mode(0o755),
                    );
                }
            }
            Ok((0, "installed".into()))
        }
    }

    /// uv 回退执行器：无 env 的官方脚本尝试退出 1；带 ghfast
    /// INSTALLER_DOWNLOAD_URL 的尝试成功并落 uv 产物。
    struct UvFallback {
        home: String,
    }

    impl ToolchainExecutor for UvFallback {
        fn run(
            &self,
            argv: &[&str],
            env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            if argv.iter().any(|a| a.contains("astral.sh")) && env_kv.is_empty() {
                return Ok((1, "connection refused".into()));
            }
            if env_kv
                .iter()
                .any(|(k, v)| k == "INSTALLER_DOWNLOAD_URL" && v.contains("ghfast.top"))
            {
                let p = std::path::Path::new(&self.home).join(".local/bin/uv");
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"#!/bin/sh\n").unwrap();
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
                return Ok((0, "installed via mirror".into()));
            }
            Ok((0, "ok".into()))
        }
    }

    /// rustup 回退执行器：带清华 RUSTUP_DIST_SERVER env 的尝试退出 1；
    /// 无 env 的尝试成功并落 cargo 产物。
    struct TunaFallback {
        cargo_bin: String,
    }

    impl ToolchainExecutor for TunaFallback {
        fn run(
            &self,
            _argv: &[&str],
            env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            if env_kv
                .iter()
                .any(|(k, v)| k == "RUSTUP_DIST_SERVER" && v.contains("tuna"))
            {
                return Ok((1, "mirror timeout".into()));
            }
            let p = std::path::Path::new(&self.cargo_bin);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
            Ok((0, "ok".into()))
        }
    }

    // ---- 测试基建 ----

    /// 临时 HOME（自管清理）。
    struct TempHome {
        dir: std::path::PathBuf,
    }
    impl TempHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agenthub-tc-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("创建临时 HOME");
            Self { dir }
        }
        fn path(&self) -> String {
            self.dir.to_string_lossy().into_owned()
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// 轮询任务直到非 running（mock 秒回；5s 兜底防死等）。
    async fn wait_task(state: &ToolchainState, task_id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let resp = handle(
                state,
                HttpMethod::Get,
                &["install", "tasks", task_id],
                serde_json::Value::Null,
            )
            .await
            .expect("handle 不应 Err");
            assert_eq!(resp.status, 200, "task body: {resp:?}");
            if resp.body["status"] != "running" {
                return resp.body;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("任务 {task_id} 5s 未完成");
    }

    async fn post_install(state: &ToolchainState, name: &str) -> ApiResponse {
        handle(
            state,
            HttpMethod::Post,
            &["install"],
            serde_json::json!({ "name": name }),
        )
        .await
        .expect("handle 不应 Err")
    }

    fn log_joined(task: &serde_json::Value) -> String {
        task["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn route_specs_declare_two_toolchain_endpoints() {
        let specs = route_specs();
        assert_eq!(specs.len(), 2, "应有 2 条工具链路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "agenthub"));
        let post = specs
            .iter()
            .find(|s| s.method == HttpMethod::Post)
            .expect("应有 POST");
        assert!(post.requires_auth);
        assert_eq!(post.required_roles, vec!["admin".to_string()]);
        assert_eq!(post.path, "/api/v1/agenthub/toolchain/install");
        let get = specs
            .iter()
            .find(|s| s.method == HttpMethod::Get)
            .expect("应有 GET");
        assert!(!get.requires_auth);
        assert_eq!(get.path, "/api/v1/agenthub/toolchain/install/tasks/:id");
    }

    // ---- 请求体校验 / 404 ----

    #[tokio::test]
    async fn install_rejects_unknown_toolchain_names() {
        let home = TempHome::new("reject");
        let state = ToolchainState::with_executor(home.path(), Arc::new(AlwaysOk));
        for bad in ["npm", "curl", "bogus", ""] {
            let resp = post_install(&state, bad).await;
            assert_eq!(resp.status, 400, "name={bad:?} 应 400: {resp:?}");
        }
    }

    #[tokio::test]
    async fn missing_task_returns_404() {
        let home = TempHome::new("t404");
        let state = ToolchainState::with_executor(home.path(), Arc::new(AlwaysOk));
        let resp = handle(
            &state,
            HttpMethod::Get,
            &["install", "tasks", "tctask-999"],
            serde_json::Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 404);
        // 未覆盖路由同样 404
        let resp = handle(
            &state,
            HttpMethod::Put,
            &["install"],
            serde_json::Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- node 安装：镜像优先 + 官方回退 + 产物校验 + .bashrc ----

    #[tokio::test]
    async fn node_install_prefers_ghfast_mirror_then_official_fallback() {
        let home = TempHome::new("node-fb");
        // ghfast 域失败 → 回退官方脚本（官方脚本那次落产物）
        let sim = SimInstall {
            trigger: "raw.githubusercontent.com/nvm-sh/nvm",
            artifacts: vec![
                format!("{}/.nvm/versions/node/v22.20.0/bin/node", home.path()),
                format!("{}/.nvm/versions/node/v22.20.0/bin/npm", home.path()),
            ],
            fail_trigger: Some("ghfast.top"),
        };
        let state = ToolchainState::with_executor(home.path(), Arc::new(sim));
        let resp = post_install(&state, "node").await;
        assert_eq!(resp.status, 202, "install body: {resp:?}");
        assert_eq!(resp.body["status"], "running");
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done", "日志: {task:?}");
        let log = log_joined(&task);
        // 主源是 ghfast 镜像脚本
        assert!(
            log.contains(&format!("curl -o- {} | bash", ghfast(NVM_INSTALL_URL_OFFICIAL))),
            "应优先 ghfast 镜像: {log}"
        );
        // 镜像失败 → 回退官方源一次
        assert!(log.contains("回退官方源重试一次"), "缺回退日志: {log}");
        assert!(
            log.contains(&format!("curl -o- {NVM_INSTALL_URL_OFFICIAL} | bash")),
            "缺官方 URL: {log}"
        );
        // nvm install --lts 走 npmmirror
        assert!(log.contains("nvm install --lts"), "缺 nvm install: {log}");
        assert!(
            log.contains(&format!("NVM_NODEJS_ORG_MIRROR={NVM_NODE_MIRROR_DEFAULT}")),
            "缺 node 镜像 env: {log}"
        );
        // .bashrc 幂等追加
        let bashrc = std::fs::read_to_string(home.dir.join(".bashrc")).expect(".bashrc 应已写");
        assert!(bashrc.contains("nvm.sh"), ".bashrc 缺 nvm 初始化: {bashrc}");
    }

    #[tokio::test]
    async fn node_install_mirror_success_skips_fallback_and_idempotent_second_time() {
        let home = TempHome::new("node-ok");
        let sim = SimInstall {
            trigger: "install.sh",
            artifacts: vec![
                format!("{}/.nvm/versions/node/v20.18.0/bin/node", home.path()),
                format!("{}/.nvm/versions/node/v20.18.0/bin/npm", home.path()),
            ],
            fail_trigger: None,
        };
        let state = ToolchainState::with_executor(home.path(), Arc::new(sim));
        let resp = post_install(&state, "node").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done");
        let log = log_joined(&task);
        assert!(!log.contains("回退官方源重试一次"), "镜像成功不应回退: {log}");

        // 幂等：产物已存在（探测命中临时 HOME 用户目录）→ 任务直接 done，不跑安装器
        let resp = post_install(&state, "node").await;
        assert_eq!(resp.status, 202, "幂等安装应 202: {resp:?}");
        assert_eq!(resp.body["status"], "done");
        let task2 = wait_task(&state, resp.body["task_id"].as_str().unwrap()).await;
        assert_eq!(task2["status"], "done");
        let log2 = log_joined(&task2);
        assert!(log2.contains("已安装"), "应提示已安装: {log2}");
        assert!(!log2.contains("nvm install"), "幂等命中不应跑安装器: {log2}");
    }

    #[tokio::test]
    async fn node_install_missing_artifact_fails_task() {
        let home = TempHome::new("node-fail");
        // AlwaysOk 不落产物 → 用户目录探测不命中 → 校验失败（error 路径）
        let state = ToolchainState::with_executor(home.path(), Arc::new(AlwaysOk));
        let resp = post_install(&state, "node").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "error");
        assert!(
            log_joined(&task).contains("仍未找到 node"),
            "应报未找到 node: {task:?}"
        );
    }

    // ---- uv 安装：官方直连 + ghfast 回退 ----

    #[tokio::test]
    async fn uv_install_official_first_then_ghfast_env_fallback() {
        let home = TempHome::new("uv-fb");
        let state = ToolchainState::with_executor(
            home.path(),
            Arc::new(UvFallback {
                home: home.path(),
            }),
        );
        let resp = post_install(&state, "uv").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done", "日志: {task:?}");
        let log = log_joined(&task);
        assert!(log.contains(&format!("curl -LsSf {UV_INSTALL_URL_DEFAULT} | sh")));
        assert!(log.contains("回退 ghfast 镜像重试一次"), "缺回退日志: {log}");
        assert!(
            log.contains(&format!("INSTALLER_DOWNLOAD_URL={}", ghfast(UV_RELEASE_BASE_OFFICIAL))),
            "回退应带 ghfast 下载源 env: {log}"
        );
    }

    #[tokio::test]
    async fn uv_install_success_first_attempt() {
        let home = TempHome::new("uv-ok");
        let sim = SimInstall {
            trigger: "astral.sh",
            artifacts: vec![format!("{}/.local/bin/uv", home.path())],
            fail_trigger: None,
        };
        let state = ToolchainState::with_executor(home.path(), Arc::new(sim));
        let resp = post_install(&state, "uv").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done");
        assert!(!log_joined(&task).contains("回退"), "直连成功不应回退");
    }

    // ---- cargo 安装：清华镜像 env 优先 + 官方回退 ----

    #[tokio::test]
    async fn cargo_install_uses_tuna_mirror_env() {
        let home = TempHome::new("cargo");
        let sim = SimInstall {
            trigger: "sh.rustup.rs",
            artifacts: vec![format!("{}/.cargo/bin/cargo", home.path())],
            fail_trigger: None,
        };
        let state = ToolchainState::with_executor(home.path(), Arc::new(sim));
        let resp = post_install(&state, "cargo").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done", "日志: {task:?}");
        let log = log_joined(&task);
        assert!(log.contains("sh.rustup.rs"), "缺 rustup 脚本: {log}");
        assert!(log.contains("--profile minimal"), "缺 profile 参数: {log}");
        assert!(
            log.contains(&format!("RUSTUP_DIST_SERVER={RUSTUP_DIST_MIRROR}")),
            "缺清华 dist 镜像 env: {log}"
        );
        assert!(
            log.contains(&format!("RUSTUP_UPDATE_ROOT={RUSTUP_UPDATE_MIRROR}")),
            "缺清华 rustup-update 镜像 env: {log}"
        );
        assert!(!log.contains("回退官方源"), "镜像成功不应回退: {log}");
    }

    #[tokio::test]
    async fn cargo_install_tuna_failure_falls_back_to_official() {
        let home = TempHome::new("cargo-fb");
        let state = ToolchainState::with_executor(
            home.path(),
            Arc::new(TunaFallback {
                cargo_bin: format!("{}/.cargo/bin/cargo", home.path()),
            }),
        );
        let resp = post_install(&state, "cargo").await;
        assert_eq!(resp.status, 202);
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["status"], "done", "日志: {task:?}");
        assert!(
            log_joined(&task).contains("回退官方源重试一次"),
            "缺回退日志: {task:?}"
        );
    }

    // ---- 409 重复任务守卫 ----

    #[tokio::test]
    async fn duplicate_running_install_returns_409() {
        let home = TempHome::new("dup");
        let state = ToolchainState::with_executor(home.path(), Arc::new(AlwaysOk));
        // 手工塞一个 running 任务（等价安装线程进行中）
        state
            .tasks
            .lock()
            .unwrap()
            .insert(
                "tctask-x".into(),
                ToolchainTask {
                    id: "tctask-x".into(),
                    toolchain: "uv".into(),
                    status: "running".into(),
                    log: vec![],
                    started_at: now_epoch(),
                    finished_at: None,
                },
            );
        let resp = post_install(&state, "uv").await;
        assert_eq!(resp.status, 409, "重复安装应 409: {resp:?}");
    }

    // ---- 任务视图 / 环形日志 ----

    #[tokio::test]
    async fn task_detail_contains_log_array_and_fields() {
        let home = TempHome::new("detail");
        let sim = SimInstall {
            trigger: "astral.sh",
            artifacts: vec![format!("{}/.local/bin/uv", home.path())],
            fail_trigger: None,
        };
        let state = ToolchainState::with_executor(home.path(), Arc::new(sim));
        let resp = post_install(&state, "uv").await;
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task(&state, &task_id).await;
        assert_eq!(task["id"], task_id);
        assert_eq!(task["toolchain"], "uv");
        assert!(task["log"].as_array().is_some(), "详情应带日志数组");
        assert!(!task["log"].as_array().unwrap().is_empty(), "日志应非空");
        assert!(task["started_at"].as_i64().is_some());
        assert!(task["finished_at"].is_i64(), "完成后应带 finished_at");
    }

    #[test]
    fn task_log_ring_buffer_caps_at_200_lines() {
        let tasks: Arc<Mutex<HashMap<String, ToolchainTask>>> =
            Arc::new(Mutex::new(HashMap::new()));
        tasks.lock().unwrap().insert(
            "tctask-1".into(),
            ToolchainTask {
                id: "tctask-1".into(),
                toolchain: "node".into(),
                status: "running".into(),
                log: Vec::new(),
                started_at: 0,
                finished_at: None,
            },
        );
        let ctx = TaskCtx {
            task_id: "tctask-1".into(),
            toolchain: "node".into(),
            home: "/tmp".into(),
            executor: Arc::new(AlwaysOk),
            probe: Arc::new(toolchain_available),
            tasks: Arc::clone(&tasks),
        };
        for i in 0..300 {
            task_log(&ctx, &format!("line-{i}"));
        }
        let log = tasks.lock().unwrap().get("tctask-1").unwrap().log.clone();
        assert_eq!(log.len(), TASK_LOG_MAX_LINES);
        assert_eq!(log.first().unwrap(), "line-100", "应保留最后 200 行");
        assert_eq!(log.last().unwrap(), "line-299");
    }

    // ---- 纯函数 ----

    #[test]
    fn ghfast_prefixes_github_urls() {
        assert_eq!(
            ghfast("https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh"),
            "https://ghfast.top/https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh"
        );
    }

    #[test]
    fn tail_chars_keeps_suffix() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 10), "ab");
    }

    #[test]
    fn ensure_nvm_in_bashrc_is_idempotent() {
        let home = TempHome::new("bashrc");
        ensure_nvm_in_bashrc(&home.path()).expect("首次追加");
        let first = std::fs::read_to_string(home.dir.join(".bashrc")).unwrap();
        assert!(first.contains("NVM_DIR"));
        ensure_nvm_in_bashrc(&home.path()).expect("二次幂等");
        let second = std::fs::read_to_string(home.dir.join(".bashrc")).unwrap();
        assert_eq!(first, second, "已含 nvm 字样时不应重复追加");
        // nvm 安装脚本自己写的行同样命中幂等判定
        let home2 = TempHome::new("bashrc2");
        std::fs::write(
            home2.dir.join(".bashrc"),
            b"export NVM_DIR=\"$HOME/.nvm\"\n[ -s \"$NVM_DIR/nvm.sh\" ] && . \"$NVM_DIR/nvm.sh\"\n",
        )
        .unwrap();
        ensure_nvm_in_bashrc(&home2.path()).expect("已有 nvm 行");
        let content = std::fs::read_to_string(home2.dir.join(".bashrc")).unwrap();
        assert_eq!(content.matches("nvm.sh").count(), 2, "不应再追加: {content}");
    }

    #[test]
    fn toolchain_available_requires_both_node_and_npm() {
        let home = TempHome::new("avail");
        // 临时 HOME 无安装物 → 唯一可能命中的是用户目录分支（None）；
        // PATH 分支依赖宿主，本用例只验证用户目录兜底分支：
        let node = format!("{}/.nvm/versions/node/v22.1.0/bin/node", home.path());
        let npm = format!("{}/.nvm/versions/node/v22.1.0/bin/npm", home.path());
        for p in [&node, &npm] {
            std::fs::create_dir_all(std::path::Path::new(p).parent().unwrap()).unwrap();
            std::fs::write(p, b"#!/bin/sh\n").unwrap();
        }
        let found =
            toolchain_available(&home.path(), "node").expect("node+npm 应命中 nvm 兜底");
        assert!(found.contains(".nvm"), "应命中 nvm 兜底路径: {found}");
    }

    // ---- 端到端（经 AgentHubRouteHandler 的路由分发，mock 执行器）----

    #[tokio::test]
    async fn handler_dispatches_toolchain_routes_end_to_end() {
        use crate::gateway::{ApiRequest, RouteHandler};

        let home = TempHome::new("e2e");
        let sim = SimInstall {
            trigger: "astral.sh",
            artifacts: vec![format!("{}/.local/bin/uv", home.path())],
            fail_trigger: None,
        };
        let h = super::super::agenthub::AgentHubRouteHandler::with_state_file_and_toolchain(
            &format!("{}/agenthub.json", home.path()),
            ToolchainState::with_executor(home.path(), Arc::new(sim)),
        );
        // routes 挂载了 2 条工具链路由
        let routes = h.routes().await;
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/toolchain/install"),
            "缺 POST 路由: {routes:?}"
        );
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/toolchain/install/tasks/:id"),
            "缺 GET 路由: {routes:?}"
        );
        // POST 安装（走 handle 分发）
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: "/api/v1/agenthub/toolchain/install".into(),
                headers: serde_json::json!({}),
                body: serde_json::json!({"name": "uv"}),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "e2e install: {resp:?}");
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        // 轮询任务详情（走 handle 分发）
        let mut task = serde_json::Value::Null;
        for _ in 0..200 {
            let resp = h
                .handle(ApiRequest {
                    method: HttpMethod::Get,
                    path: format!("/api/v1/agenthub/toolchain/install/tasks/{task_id}"),
                    headers: serde_json::json!({}),
                    body: serde_json::Value::Null,
                    auth: None,
                })
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            if resp.body["status"] != "running" {
                task = resp.body;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(task["status"], "done", "e2e 任务应完成: {task:?}");
        // 既有 toolchains 端点不受影响（复数路径不与 toolchain 前缀冲突）
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/agenthub/toolchains".into(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
    }
}
