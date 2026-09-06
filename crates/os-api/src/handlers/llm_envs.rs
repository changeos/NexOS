//! 推理环境（vLLM Python venv）管理 —— `llm` handler 的子模块。
//!
//! 定位：把「Python venv + 指定版本 vLLM」做成可管理的**推理环境**——多环境
//! 并存、一个默认环境供实例拉起、页面一键创建/更新。机器重装后旧的
//! `/home/oem/vllm-env` 硬编码 venv 丢失，本模块用 uv 在 `~/llm-envs/<name>/`
//! 下按需重建任意多个 venv，[`crate::handlers::llm`] 的实例 spawn 改为从注册表
//! 解析默认环境的 `bin/vllm`（注册表无可用默认行时回退旧硬编码路径，向后兼容）。
//!
//! # 存储
//!
//! 复用 llm 数据库连接（`llm.db`），新表 `llm_environments`（建表幂等，见
//! [`create_env_schema`]）。status ∈ `creating|updating|ready|error`；首个创建的
//! 环境自动设为默认（`is_default=1`，切换默认走事务互斥）。
//!
//! # 异步任务
//!
//! 创建/更新是分钟级长操作（uv 下载 CPython + 装 vLLM 及 CUDA 依赖），接口
//! 立即返回 `202 {task_id}`，后台 std 线程执行；任务态存进程内
//! `Mutex<HashMap<String, EnvTask>>`（环形日志上限 200 行），前端轮询
//! `GET /environments/tasks/:id` 看进度（日志尾）。服务重启任务态即清（DB 行
//! 停在 creating/updating 的环境可再次 update 修复）。
//!
//! # 渠道（channel，2026-09-02）
//!
//! 创建/更新请求体新增可选 `channel`：`"stable"`（默认，行为与历史完全一致）
//! 或 `"nightly"`（预置示例——`uv pip install -U vllm --torch-backend=auto
//! --extra-index-url https://wheels.vllm.ai/nightly`，恒最新不钉版本）。注册表
//! 列 `channel` 随行透出（存量行 NULL 读作 stable，ALTER 幂等迁移）。详见
//! [`pip_install_argv`] 与 docs/LLM_ENVIRONMENTS.md。
//!
//! # 执行器抽象（真实数据铁律）
//!
//! 全部外部命令经 [`EnvExecutor`] 抽象：生产 [`UvExecutor`]（std::process 真实
//! 执行 + 超时 kill），测试注入 mock（cfg(test) 内定义，绝不真跑 uv/网络）。
//!
//! # REST 契约（挂进 llm.rs 的 routes()，6 条；写接口 admin）
//!
//! | method | path                                    | 动作 |
//! |--------|-----------------------------------------|------|
//! | GET    | `/api/v1/llm/environments`              | 环境列表 + default_name（公开读）|
//! | POST   | `/api/v1/llm/environments`              | 创建环境 → 202 {task_id}（admin）|
//! | POST   | `/api/v1/llm/environments/:name/update` | 更新 vLLM 版本 → 202（admin）|
//! | DELETE | `/api/v1/llm/environments/:name`        | 删行 + rm -rf venv（admin；默认环境 409）|
//! | POST   | `/api/v1/llm/environments/:name/default`| 切换默认（事务互斥，admin）|
//! | GET    | `/api/v1/llm/environments/tasks`        | 任务列表（公开读）|
//! | GET    | `/api/v1/llm/environments/tasks/:id`    | 单任务含日志尾（公开读）|
//!
//! # env 清单（全部 `NEXOS_` 前缀；详见 docs/LLM_ENVIRONMENTS.md）
//!
//! - `NEXOS_LLM_ENVS_ROOT`：venv 根目录（默认 `~/llm-envs`，不存在自动创建）
//! - `NEXOS_LLM_UV_BIN`：uv 绝对路径覆盖（默认解析链 PATH → 运行用户
//!   `~/.local/bin` → `/home/*` 多用户 glob → `/root/.local/bin` → 自动安装，
//!   2026-09-03 起多用户——Spark 实测 uv 装在 `/home/nvidia/.local/bin` 而服务
//!   跑 root；找到即可用，envs 仍装运行用户 home，语义不变）
//! - `NEXOS_LLM_UV_SCAN_ROOT`：多用户 glob 基座（默认 `/home`；测试注入用）
//! - `NEXOS_LLM_UV_INSTALL_URL`：uv 安装脚本 URL 覆盖
//! - `NEXOS_LLM_PIP_INDEX_URL`：pip/uv 镜像源（stable：透传 UV_PIP_INDEX_URL/
//!   PIP_INDEX_URL 两组 env，镜像即主源；nightly：改为追加第二个
//!   `--extra-index-url` 排在 nightly 源之后兜底 PyPI 依赖轮子，见
//!   [`nightly_extra_args`]）
//!
//! 日志：进程内一律 `eprintln!`（`[llm-env]` 前缀），不用 tracing（os-api 无
//! subscriber，tracing 无声）。

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiResponse, HttpMethod, RouteSpec};

// ----------------------------------------------------------------------------
// 常量与 env 配置
// ----------------------------------------------------------------------------

/// venv 根目录覆盖 env。
const ENVS_ROOT_ENV: &str = "NEXOS_LLM_ENVS_ROOT";

/// uv 绝对路径覆盖 env。
const UV_BIN_ENV: &str = "NEXOS_LLM_UV_BIN";

/// uv 安装脚本 URL 覆盖 env。
const UV_INSTALL_URL_ENV: &str = "NEXOS_LLM_UV_INSTALL_URL";

/// uv 多用户扫描基座覆盖 env（默认 `/home`；测试注入 tempdir 造多用户布局，
/// 特殊家目录布局的机器也可指到自己的位置——如 `/srv/home`）。
const UV_SCAN_ROOT_ENV: &str = "NEXOS_LLM_UV_SCAN_ROOT";

/// uv 多用户扫描基座（生产 `/home`）。
const UV_USER_HOMES_DEFAULT: &str = "/home";

/// pip 镜像源 env（透传给 uv pip install）。
const PIP_INDEX_ENV: &str = "NEXOS_LLM_PIP_INDEX_URL";

/// uv 官方安装脚本（`NEXOS_LLM_UV_INSTALL_URL` 可覆盖）。
const UV_INSTALL_URL_DEFAULT: &str = "https://astral.sh/uv/install.sh";

/// 渠道：稳定版（默认；安装命令与历史行为完全一致）。
const CHANNEL_STABLE: &str = "stable";

/// 渠道：nightly（预置示例；恒最新不钉版本，见 [`VLLM_NIGHTLY_INDEX`]）。
const CHANNEL_NIGHTLY: &str = "nightly";

/// vLLM 官方 nightly 轮子源（channel=nightly 的**主**源——vLLM 每日构建只发
/// 在这里，PyPI/镜像源没有；`--torch-backend=auto` 让 uv 自动选 CUDA 轮子后端）。
const VLLM_NIGHTLY_INDEX: &str = "https://wheels.vllm.ai/nightly";

/// `uv venv` 超时（下载 CPython + 建目录，30min 兜底防挂死）。
const UV_VENV_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `uv pip install vllm` 超时（vLLM + CUDA 轮子数 GB，30min 兜底；创建/更新同限）。
const UV_PIP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 版本探测（`python -c importlib.metadata`）超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// uv 自安装（curl | sh）超时。
const UV_SELF_INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// 任务日志环形上限（行）。
const TASK_LOG_MAX_LINES: usize = 200;

/// 执行器返回的 stdout+stderr 截尾上限（字符；防单条日志爆内存）。
const OUTPUT_TAIL_CHARS: usize = 4000;

/// 读非空 env。
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 家目录（`$HOME` → `/home/$USER` → `/root` 兜底；systemd 系统服务可能不带
/// HOME，不写死任何具体用户名）。
fn home_dir() -> String {
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

/// venv 根目录：`NEXOS_LLM_ENVS_ROOT` 覆盖 → `~/llm-envs`。
fn default_envs_root() -> String {
    env_non_empty(ENVS_ROOT_ENV).unwrap_or_else(|| format!("{}/llm-envs", home_dir()))
}

// ----------------------------------------------------------------------------
// 校验（纯函数）
// ----------------------------------------------------------------------------

/// 环境名合法性：`^[a-z0-9][a-z0-9-]{0,31}$`（小写字母/数字/连字符，1-32 字符，
/// 防路径穿越与奇怪目录名）。
#[must_use]
pub fn valid_env_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return false;
    }
    let first_ok = b[0].is_ascii_lowercase() || b[0].is_ascii_digit();
    first_ok
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// vLLM 版本号合法性：`latest` 或以数字开头的版本串（`0.26.0`/`0.11.0rc1` 等；
/// 字符集限字母数字 `.+-`，长度 ≤ 32——作为单个 argv 元素传给 uv，无 shell 注入面，
/// 校验只挡手滑）。
#[must_use]
pub fn valid_vllm_version(v: &str) -> bool {
    let t = v.trim();
    if t == "latest" {
        return true;
    }
    !t.is_empty()
        && t.len() <= 32
        && t.chars().next().is_some_and(|c| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Python 版本合法性：`3.10` 形如 `^3\.\d{1,2}$`。
#[must_use]
pub fn valid_python_version(v: &str) -> bool {
    let t = v.trim();
    let mut parts = t.split('.');
    matches!(parts.next(), Some("3"))
        && parts
            .next()
            .is_some_and(|m| (1..=2).contains(&m.len()) && m.chars().all(|c| c.is_ascii_digit()))
        && parts.next().is_none()
}

/// 渠道合法性：仅 `stable`（默认）| `nightly`（大小写敏感；其余 400）。
#[must_use]
pub fn valid_channel(c: &str) -> bool {
    matches!(c, CHANNEL_STABLE | CHANNEL_NIGHTLY)
}

/// 请求体 channel 规范化：缺省/空白 → [`CHANNEL_STABLE`]；非法值 Err（caller 400）。
fn parse_channel(raw: Option<&str>) -> Result<String, String> {
    let c = raw
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(CHANNEL_STABLE);
    if valid_channel(c) {
        Ok(c.to_string())
    } else {
        Err(format!("channel 非法（{CHANNEL_STABLE} 或 {CHANNEL_NIGHTLY}）: {c}"))
    }
}

/// vLLM 安装 spec：`latest`/空 → `vllm`，否则 `vllm==<ver>`（stable 渠道用）。
fn vllm_spec(version: &str) -> String {
    let v = version.trim();
    if v.is_empty() || v == "latest" {
        "vllm".to_string()
    } else {
        format!("vllm=={v}")
    }
}

/// channel=nightly 的附加 argv：`--torch-backend=auto`（uv 按 CUDA 版本自动选
/// torch 轮子后端）+ nightly 轮子源 +（可选）镜像兜底源。
///
/// **顺序语义**：`mirror`（`NEXOS_LLM_PIP_INDEX_URL` 的值）设置时追加为第二个
/// `--extra-index-url`，排在 nightly 源**之后**——uv 默认 first-index 策略下
/// 顺序即优先级：vLLM nightly 轮子只在上游 nightly 源（必须命中），其余
/// PyPI 依赖轮子 nightly 源没有、落到镜像/官方源下载（镜像做 PyPI 兜底）。
/// 注意**不走** `UV_PIP_INDEX_URL` env：那会把镜像顶成主源、改写优先级
/// （stable 渠道才那样透传，见 [`pip_index_kv_for`]）。
fn nightly_extra_args(mirror: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--torch-backend=auto".into(),
        "--extra-index-url".into(),
        VLLM_NIGHTLY_INDEX.to_string(),
    ];
    if let Some(url) = mirror {
        args.push("--extra-index-url".into());
        args.push(url.to_string());
    }
    args
}

/// stable 渠道的镜像源透传 env（`NEXOS_LLM_PIP_INDEX_URL` → uv/pip 两套变量；
/// 镜像即主源——现状行为，零变化）。
fn pip_index_kv_for(mirror: Option<&str>) -> Vec<(String, String)> {
    match mirror {
        Some(url) => vec![
            ("UV_PIP_INDEX_URL".to_string(), url.to_string()),
            ("PIP_INDEX_URL".to_string(), url.to_string()),
        ],
        None => vec![],
    }
}

/// 组装 `uv pip install` 安装命令 argv（create/update 共用；纯函数便于测试）。
///
/// - **stable**（现状零变化）：`uv pip install [-U] --python <py> <spec>`，
///   spec 见 [`vllm_spec`]（`vllm` 或 `vllm==<ver>`）；镜像源走 env_kv 透传
///   （镜像即主源）。create 不带 `-U`、update 带——与历史命令逐字一致。
/// - **nightly**（预置示例，基于用户点名命令
///   `uv pip install -U vllm --torch-backend=auto --extra-index-url
///   https://wheels.vllm.ai/nightly`）：恒 `-U`、恒裸 `vllm`（**不钉版本**——
///   nightly 源取最新）、`--python` 指环境内解释器，再接
///   [`nightly_extra_args`]；env_kv 恒空（镜像改走第二个 extra-index-url，
///   顺序语义见该函数注释）。
///
/// 返回 `(argv, env_kv)`；`upgrade` 仅对 stable 生效（nightly 恒 -U）。
fn pip_install_argv(
    uv: &str,
    python: &str,
    version: &str,
    channel: &str,
    upgrade: bool,
    mirror: Option<&str>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut argv: Vec<String> = vec![uv.to_string(), "pip".into(), "install".into()];
    if channel == CHANNEL_NIGHTLY {
        argv.push("--python".into());
        argv.push(python.to_string());
        argv.push("-U".into());
        argv.push("vllm".into());
        argv.extend(nightly_extra_args(mirror));
        (argv, vec![])
    } else {
        if upgrade {
            argv.push("-U".into());
        }
        argv.push("--python".into());
        argv.push(python.to_string());
        argv.push(vllm_spec(version));
        (argv, pip_index_kv_for(mirror))
    }
}

/// 环境根目录 + 合法名 → venv 路径。
fn env_path(root: &str, name: &str) -> String {
    format!("{root}/{name}")
}

/// 环境内 python 解释器绝对路径。
fn env_python(path: &str) -> String {
    format!("{path}/bin/python")
}

// ----------------------------------------------------------------------------
// EnvExecutor 抽象（生产真实执行 / 测试注入 mock）
// ----------------------------------------------------------------------------

/// 外部命令执行器抽象：生产 [`UvExecutor`] 真跑子进程，测试注入 mock。
///
/// 返回 `(退出码, stdout+stderr 截尾)`；命令无法启动/超时返回 Err（带原因）。
pub trait EnvExecutor: Send + Sync {
    /// 执行 `argv[0] argv[1..]`，注入 `env_kv` 环境变量。
    fn run(&self, argv: &[&str], env_kv: &[(String, String)]) -> Result<(i32, String), String>;
}

/// 生产执行器：std::process 真实执行，按命令类别限时（防 uv 挂死），超时 kill。
struct UvExecutor;

impl UvExecutor {
    /// 按命令类别选超时：`venv` 建目录 30min / `pip install` 30min / python
    /// `-c` 版本探测 2min / 其余（uv 自安装 curl|sh）5min。
    fn timeout_for(argv: &[&str]) -> Duration {
        let has = |a: &str| argv.iter().skip(1).any(|x| *x == a);
        if has("venv") {
            UV_VENV_TIMEOUT
        } else if has("pip") {
            UV_PIP_TIMEOUT
        } else if argv.first().is_some_and(|a| a.ends_with("python")) && has("-c") {
            PROBE_TIMEOUT
        } else {
            UV_SELF_INSTALL_TIMEOUT
        }
    }
}

impl EnvExecutor for UvExecutor {
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
        match rx.recv_timeout(Self::timeout_for(argv)) {
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let code = out.status.code().unwrap_or(-1);
                Ok((code, tail_chars(&text, OUTPUT_TAIL_CHARS)))
            }
            Ok(Err(e)) => Err(format!("等待 {} 完成（pid {pid}）失败: {e}", argv[0])),
            Err(_) => {
                eprintln!(
                    "[llm-env] 命令超时（>{:?}），kill pid {pid}: {}",
                    Self::timeout_for(argv),
                    argv.join(" ")
                );
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                // 等 waiter 线程收尸（kill 后管道关闭，wait_with_output 很快返回）
                let _ = rx.recv_timeout(Duration::from_secs(10));
                Err(format!(
                    "命令超时（>{:?}）已 kill: {}",
                    Self::timeout_for(argv),
                    argv.join(" ")
                ))
            }
        }
    }
}

/// 字符串截尾（保留最后 n 个字符）。
fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().skip(s.chars().count() - n).collect()
}

/// 路径是否可执行文件（Unix exec 位；探测 uv 用）。
fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// `llm_environments` 表一行（GET /environments 列表元素）。
#[derive(Debug, Clone, Serialize)]
pub struct EnvRow {
    pub name: String,
    /// venv 绝对路径（`<root>/<name>`）。
    pub path: String,
    pub python_version: Option<String>,
    /// 请求的 vLLM 版本（`latest` 或具体版本号；nightly 渠道恒 `latest`）。
    pub vllm_version_requested: Option<String>,
    /// 探测到的已装版本。
    pub vllm_version_installed: Option<String>,
    /// 安装渠道：`stable`（默认）| `nightly`（存量行 NULL 读作 stable）。
    pub channel: String,
    pub is_default: bool,
    /// `creating` / `updating` / `ready` / `error`。
    pub status: String,
    pub size_bytes: u64,
    /// Unix epoch 秒。
    pub created_at: i64,
    pub updated_at: i64,
    pub last_error: Option<String>,
}

/// 后台任务（进程内态；重启即清，DB 行才是环境真值）。
#[derive(Debug, Clone)]
pub struct EnvTask {
    pub id: String,
    /// `create` / `update`。
    pub kind: String,
    pub env_name: String,
    /// `running` / `done` / `error`。
    pub status: String,
    /// 环形日志（上限 [`TASK_LOG_MAX_LINES`] 行）。
    pub log: Vec<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// 任务摘要（GET tasks 列表元素；日志只在单任务详情返回）。
#[derive(Debug, Serialize)]
struct TaskSummary {
    id: String,
    kind: String,
    env_name: String,
    status: String,
    started_at: i64,
    finished_at: Option<i64>,
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（llm.db · llm_environments 表）
// ----------------------------------------------------------------------------

/// 建表（IF NOT EXISTS；由 llm.rs 的 create_schema 一并调用，同连接幂等）。
pub fn create_env_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_environments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE NOT NULL,
            path TEXT NOT NULL,
            python_version TEXT,
            vllm_version_requested TEXT,
            vllm_version_installed TEXT,
            channel TEXT,
            is_default INTEGER DEFAULT 0,
            status TEXT DEFAULT 'creating',
            created_at INTEGER,
            updated_at INTEGER,
            last_error TEXT,
            size_bytes INTEGER DEFAULT 0
        );",
    )?;
    // 迁移：2026-09-02 起新增 channel（stable|nightly）。CREATE IF NOT EXISTS
    // 不会给已存在的表补列；列已存在时 ALTER 报 duplicate column，忽略即可
    // （幂等，llm.rs 的 env_name/launch_command 同款惯例）。存量行 NULL 由
    // env_list 读取时兜底为 'stable'。
    let _ = conn.execute(
        "ALTER TABLE llm_environments ADD COLUMN channel TEXT",
        [],
    );
    Ok(())
}

/// 当前 Unix epoch 秒。
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 插入环境行（status=creating；首个环境 is_default=1 由 caller 决定后传入）。
fn env_insert(
    conn: &Connection,
    name: &str,
    path: &str,
    python_version: &str,
    vllm_requested: &str,
    channel: &str,
    is_default: bool,
) -> rusqlite::Result<()> {
    let now = now_epoch();
    conn.execute(
        "INSERT INTO llm_environments
         (name,path,python_version,vllm_version_requested,vllm_version_installed,
          channel,is_default,status,created_at,updated_at,last_error,size_bytes)
         VALUES (?1,?2,?3,?4,NULL,?5,?6,'creating',?7,?8,NULL,0)",
        params![
            name,
            path,
            python_version,
            vllm_requested,
            channel,
            i64::from(is_default),
            now,
            now
        ],
    )?;
    Ok(())
}

/// 更新状态（+可选错误信息）。
fn env_set_status(
    conn: &Connection,
    name: &str,
    status: &str,
    err: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE llm_environments SET status=?1, last_error=?2, updated_at=?3 WHERE name=?4",
        params![status, err, now_epoch(), name],
    )?;
    Ok(())
}

/// 标记 ready：记录已装版本 + 占用大小。
fn env_set_ready(
    conn: &Connection,
    name: &str,
    installed: &str,
    size_bytes: u64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE llm_environments
         SET status='ready', vllm_version_installed=?1, size_bytes=?2, last_error=NULL, updated_at=?3
         WHERE name=?4",
        params![installed, size_bytes as i64, now_epoch(), name],
    )?;
    Ok(())
}

/// 更新任务开始时记录目标（版本 + 渠道——update 可 nightly↔stable 切换重装）。
fn env_set_update_target(
    conn: &Connection,
    name: &str,
    requested: &str,
    channel: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE llm_environments SET vllm_version_requested=?1, channel=?2, updated_at=?3 WHERE name=?4",
        params![requested, channel, now_epoch(), name],
    )?;
    Ok(())
}

/// 删行。
fn env_delete_row(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM llm_environments WHERE name=?1", params![name])?;
    Ok(())
}

/// 单行读取。
fn env_get(conn: &Connection, name: &str) -> Option<EnvRow> {
    env_list(conn).into_iter().find(|r| r.name == name)
}

/// 全量行列表（按 id 序，即创建顺序；channel 列 NULL 兜底 stable）。
fn env_list(conn: &Connection) -> Vec<EnvRow> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT name,path,python_version,vllm_version_requested,vllm_version_installed,
                channel,is_default,status,created_at,updated_at,last_error,size_bytes
         FROM llm_environments ORDER BY id",
    ) else {
        return vec![];
    };
    let rows = stmt.query_map([], |row| {
        Ok(EnvRow {
            name: row.get(0)?,
            path: row.get(1)?,
            python_version: row.get(2)?,
            vllm_version_requested: row.get(3)?,
            vllm_version_installed: row.get(4)?,
            channel: row
                .get::<_, Option<String>>(5)?
                .unwrap_or_else(|| CHANNEL_STABLE.to_string()),
            is_default: row.get::<_, i64>(6)? != 0,
            status: row.get(7)?,
            created_at: row.get::<_, i64>(8)?.max(0),
            updated_at: row.get::<_, i64>(9)?.max(0),
            last_error: row.get(10)?,
            size_bytes: row.get::<_, Option<i64>>(11)?.unwrap_or(0).max(0) as u64,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => vec![],
    }
}

/// 默认就绪环境（is_default=1 且 status=ready；实例拉起用，不满足返回 None）。
#[must_use]
pub fn default_ready_env(conn: &Connection) -> Option<EnvRow> {
    env_list(conn)
        .into_iter()
        .find(|r| r.is_default && r.status == "ready")
}

/// 按名取环境行（llm.rs 实例拉起解析指定环境用）。
#[must_use]
pub fn env_row_by_name(conn: &Connection, name: &str) -> Option<EnvRow> {
    env_get(conn, name)
}

/// 环境行数（首个创建自动默认的判定）。
fn env_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM llm_environments", [], |r| r.get(0))
        .unwrap_or(0)
}

/// 切换默认（事务内互斥：先全清再单设，保证至多一行 is_default=1）。
fn env_set_default(conn: &mut Connection, name: &str) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("UPDATE llm_environments SET is_default=0", [])?;
    let n = tx.execute(
        "UPDATE llm_environments SET is_default=1, updated_at=?1 WHERE name=?2",
        params![now_epoch(), name],
    )?;
    if n == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    tx.commit()?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 目录工具
// ----------------------------------------------------------------------------

/// 递归求目录占用字节数（不跟符号链接；不存在返回 0）。
fn dir_size(path: &Path) -> u64 {
    let md = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if md.is_file() {
        return md.len();
    }
    if !md.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            total += dir_size(&e.path());
        }
    }
    total
}

// ----------------------------------------------------------------------------
// LlmEnvState：路由态 + 任务注册表
// ----------------------------------------------------------------------------

/// 推理环境管理态（嵌入 [`crate::handlers::llm::LlmRouteHandler`]，与其共享
/// 同一条 llm.db 连接）。
pub struct LlmEnvState {
    /// 共享 llm.db 连接（与实例表同库；后台任务线程也经它写状态）。
    db: Arc<Mutex<Connection>>,
    /// 进程内任务注册表（id → 任务；重启即清）。
    tasks: Arc<Mutex<HashMap<String, EnvTask>>>,
    /// 任务 id 计数器。
    task_seq: AtomicU64,
    /// 命令执行器（生产 UvExecutor；测试注入 mock）。
    executor: Arc<dyn EnvExecutor>,
    /// venv 根目录。
    envs_root: String,
    /// 固定 uv 路径（测试注入确定性路径；None=生产解析链）。
    uv_bin: Option<String>,
}

impl LlmEnvState {
    /// 生产构造：UvExecutor + 默认根目录 + uv 解析链。
    #[must_use]
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self {
            db,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            task_seq: AtomicU64::new(0),
            executor: Arc::new(UvExecutor),
            envs_root: default_envs_root(),
            uv_bin: None,
        }
    }

    /// 测试注入构造：mock 执行器 + 临时根目录 + 固定 uv 路径（绝不真跑 uv/网络）。
    #[must_use]
    pub fn with_executor(
        db: Arc<Mutex<Connection>>,
        envs_root: String,
        executor: Arc<dyn EnvExecutor>,
        uv_bin: Option<String>,
    ) -> Self {
        Self {
            db,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            task_seq: AtomicU64::new(0),
            executor,
            envs_root,
            uv_bin,
        }
    }

    /// 共享 db 连接句柄（llm.rs 的 LlmRouteHandler 与本状态共用同一条连接）。
    #[must_use]
    pub fn db_handle(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.db)
    }

    /// 环境列表快照（GET /environments 用）。
    fn rows(&self) -> Vec<EnvRow> {
        match self.db.lock() {
            Ok(conn) => env_list(&conn),
            Err(_) => vec![],
        }
    }

    /// 该环境是否有 running 任务（创建/更新互斥 + 删除守卫用）。
    fn has_running_task(&self, env_name: &str) -> bool {
        self.tasks
            .lock()
            .map(|m| {
                m.values()
                    .any(|t| t.env_name == env_name && t.status == "running")
            })
            .unwrap_or(false)
    }
}

/// 任务线程共享态（执行 job 所需全部句柄）。
struct TaskCtx {
    task_id: String,
    env_name: String,
    envs_root: String,
    uv_bin: Option<String>,
    executor: Arc<dyn EnvExecutor>,
    tasks: Arc<Mutex<HashMap<String, EnvTask>>>,
    db: Arc<Mutex<Connection>>,
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
fn task_finish(ctx: &TaskCtx, kind: &str, status: &str, line: &str) {
    task_log(ctx, line);
    if let Ok(mut tasks) = ctx.tasks.lock() {
        if let Some(t) = tasks.get_mut(&ctx.task_id) {
            t.status = status.to_string();
            t.finished_at = Some(now_epoch());
        }
    }
    eprintln!(
        "[llm-env] 任务{}：{}（{kind} 环境 {}）",
        if status == "done" { "完成" } else { "失败" },
        ctx.task_id,
        ctx.env_name
    );
}

/// 经执行器跑一条命令：命令行 + 输出截尾进任务日志；非 0 退出码返回 Err。
fn run_logged(ctx: &TaskCtx, argv: &[&str], env_kv: &[(String, String)]) -> Result<String, String> {
    task_log(ctx, &format!("$ {}", argv.join(" ")));
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

/// uv 定位链（探测结果逐环写任务日志——找到于哪个路径可追溯）：
///
/// 1. 固定注入（测试 `with_executor` 的 uv_bin，直接返回不查 FS）；
/// 2. `NEXOS_LLM_UV_BIN`（显式覆盖，要求可执行）；
/// 3. **PATH 扫描**（服务环境继承的任何 uv）；
/// 4. 运行用户 `~/.local/bin/uv`（uv 官方安装器默认落点；HOME 缺失时由
///    [`home_dir`] 推 `/home/$USER` → `/root`）；
/// 5. **多用户 glob：`<user_homes>/*/.local/bin/uv`**（2026-09-03：Spark 实测——
///    服务进程跑 root（home=/root）、uv 装在 `/home/nvidia/.local/bin`，上一版
///    只看运行用户 home 即漏。找到即可用：uv 二进制跨用户调用没问题，envs 仍装到
///    运行用户 home 语义不变）；
/// 6. root 兜底 `/root/.local/bin/uv`（HOME 未设且非 root 场景；与 4 重合时跳过）；
/// 7. 自动安装（curl|sh 装进运行用户 `~/.local/bin`，走执行器、日志留痕）→
///    复查 `~/.local/bin/uv`。
///
/// 环境读取（PATH/HOME/`NEXOS_LLM_UV_SCAN_ROOT`）全部在 [`locate_uv`] 包装里
/// 解析后以参数传入 [`locate_uv_in`]——内核**不读进程 env**，测试注入合成值。
/// （不设 env 守卫改 PATH/HOME：那些是全局变量，同进程并行跑的 provisioning/
/// http 等测试 spawn git/ssh 会读，进程内改写会交叉污染。）
fn locate_uv(ctx: &TaskCtx) -> Result<String, String> {
    let path_dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    let user_homes =
        env_non_empty(UV_SCAN_ROOT_ENV).unwrap_or_else(|| UV_USER_HOMES_DEFAULT.to_string());
    locate_uv_in(&path_dirs, &home_dir(), &user_homes, "/root/.local/bin/uv", ctx)
}

/// [`locate_uv`] 的参数化内核（不读进程 env；测试注入合成值）。
///
/// - `path_dirs`：PATH 拆分后的目录列表（空 = 跳过 PATH 环）；
/// - `home`：运行用户家目录（`~/.local/bin/uv` 落点 + 安装复查点）；
/// - `user_homes`：多用户 glob 基座（生产 `/home`）；
/// - `root_fallback`：root 显式兜底路径（生产 `/root/.local/bin/uv`）。
fn locate_uv_in(
    path_dirs: &[String],
    home: &str,
    user_homes: &str,
    root_fallback: &str,
    ctx: &TaskCtx,
) -> Result<String, String> {
    if let Some(fixed) = ctx.uv_bin.as_deref() {
        task_log(ctx, &format!("使用注入的 uv 路径：{fixed}"));
        return Ok(fixed.to_string());
    }
    if let Some(p) = env_non_empty(UV_BIN_ENV) {
        if is_executable(&p) {
            task_log(ctx, &format!("使用 {UV_BIN_ENV}={p}"));
            return Ok(p);
        }
        task_log(ctx, &format!("{UV_BIN_ENV}={p} 不可执行，继续探测"));
    }
    // PATH 扫描
    for dir in path_dirs {
        let cand = format!("{dir}/uv");
        if is_executable(&cand) {
            task_log(ctx, &format!("在 PATH 中找到 uv：{cand}"));
            return Ok(cand);
        }
    }
    // 运行用户 ~/.local/bin/uv → 多用户 glob → root 兜底
    let local_uv = format!("{home}/.local/bin/uv");
    if is_executable(&local_uv) {
        task_log(ctx, &format!("使用运行用户落点 {local_uv}"));
        return Ok(local_uv);
    }
    if let Some(cand) = scan_users_local_bin_uv(user_homes).first() {
        task_log(ctx, &format!("多用户 ~/.local/bin 扫描找到 uv：{cand}"));
        return Ok(cand.clone());
    }
    if root_fallback != local_uv && is_executable(root_fallback) {
        task_log(ctx, &format!("使用 root 兜底落点 {root_fallback}"));
        return Ok(root_fallback.to_string());
    }
    // 自动安装（真实执行 curl|sh；测试用 mock executor 不会真联网）
    let url =
        env_non_empty(UV_INSTALL_URL_ENV).unwrap_or_else(|| UV_INSTALL_URL_DEFAULT.to_string());
    task_log(ctx, &format!("全链未命中 uv，自动安装：curl -LsSf {url} | sh"));
    run_logged(ctx, &["sh", "-c", &format!("curl -LsSf {url} | sh")], &[])?;
    if is_executable(&local_uv) {
        task_log(ctx, &format!("uv 安装完成：{local_uv}"));
        return Ok(local_uv);
    }
    Err("uv 自动安装后仍未在 ~/.local/bin/uv 找到可执行文件".to_string())
}

/// glob `<user_homes>/*/.local/bin/uv`（按用户名排序，只收带执行位的普通文件）。
///
/// **多用户语义**（Spark 实测缺陷的修复核心）：uv 官方安装器装进**交互用户**的
/// `~/.local/bin`，而本服务进程常以别的用户（root）运行——只看运行用户 home 必漏。
/// `user_homes` 参数化：生产 [`UV_USER_HOMES_DEFAULT`]（`/home`），测试注入
/// tempdir 造假多用户布局（不碰真机 /home）。
fn scan_users_local_bin_uv(user_homes: &str) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(user_homes) else {
        return Vec::new();
    };
    let mut users: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    users.sort();
    users
        .into_iter()
        .map(|u| format!("{user_homes}/{u}/.local/bin/uv"))
        .filter(|p| is_executable(p))
        .collect()
}

/// 探测环境内已装 vLLM 版本：`<env>/bin/python -c "import importlib.metadata..."`
/// （取输出最后一个非空行——uv/pip 可能在 stderr 打 warning）。
fn probe_installed_version(ctx: &TaskCtx, python: &str) -> Result<String, String> {
    let out = run_logged(
        ctx,
        &[
            python,
            "-c",
            "import importlib.metadata;print(importlib.metadata.version('vllm'))",
        ],
        &[],
    )?;
    out.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .ok_or_else(|| "版本探测无输出".to_string())
}

/// 创建任务线程体：建根目录 → uv venv → uv pip install（stable 钉版本 /
/// nightly 预置示例命令）→ 探测版本 → 递归求大小 → DB 标 ready。任一步失败：
/// DB 标 error + last_error。
fn run_create_task(
    ctx: TaskCtx,
    python_version: String,
    vllm_version: String,
    channel: String,
) -> Result<(), String> {
    let path = env_path(&ctx.envs_root, &ctx.env_name);
    task_log(
        &ctx,
        &format!(
            "创建推理环境 {}（Python {python_version}，vLLM {vllm_version}，渠道 {channel}）→ {path}",
            ctx.env_name
        ),
    );
    // 1. 根目录（不存在自动创建）
    std::fs::create_dir_all(&ctx.envs_root)
        .map_err(|e| format!("创建根目录 {} 失败: {e}", ctx.envs_root))?;
    // 2. uv 定位（含自动安装）
    let uv = locate_uv(&ctx)?;
    // 3. uv venv（uv 自动下载对应 CPython）
    run_logged(
        &ctx,
        &[uv.as_str(), "venv", "--python", &python_version, &path],
        &[],
    )?;
    // 4. uv pip install（stable：vllm[==ver] + 镜像 env_kv；nightly：-U vllm
    //    --torch-backend=auto --extra-index-url <nightly> [+ 镜像兜底]，见
    //    pip_install_argv——run_logged 会把完整命令行记进任务日志）
    let python = env_python(&path);
    let mirror = env_non_empty(PIP_INDEX_ENV);
    let (argv, kv) = pip_install_argv(&uv, &python, &vllm_version, &channel, false, mirror.as_deref());
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    run_logged(&ctx, &argv_ref, &kv)?;
    // 5. 探测已装版本
    let installed = probe_installed_version(&ctx, &python)?;
    // 6. 大小
    let size = dir_size(Path::new(&path));
    // 7. 落库 ready
    {
        let conn = ctx
            .db
            .lock()
            .map_err(|_| "llm.db 连接锁 poisoned".to_string())?;
        env_set_ready(&conn, &ctx.env_name, &installed, size)
            .map_err(|e| format!("写库失败: {e}"))?;
    }
    task_log(&ctx, &format!("vLLM {installed} 就绪（{size} 字节）"));
    Ok(())
}

/// 更新任务线程体：uv pip install -U（渠道可 nightly↔stable 切换重装）→
/// 探测 → 大小 → ready。
fn run_update_task(ctx: TaskCtx, vllm_version: String, channel: String) -> Result<(), String> {
    let row_path = {
        let conn = ctx
            .db
            .lock()
            .map_err(|_| "llm.db 连接锁 poisoned".to_string())?;
        env_get(&conn, &ctx.env_name)
            .ok_or_else(|| format!("环境 {} 不存在", ctx.env_name))?
            .path
    };
    task_log(
        &ctx,
        &format!(
            "更新推理环境 {}（vLLM → {vllm_version}，渠道 → {channel}）",
            ctx.env_name
        ),
    );
    let uv = locate_uv(&ctx)?;
    let python = env_python(&row_path);
    let mirror = env_non_empty(PIP_INDEX_ENV);
    let (argv, kv) = pip_install_argv(&uv, &python, &vllm_version, &channel, true, mirror.as_deref());
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    run_logged(&ctx, &argv_ref, &kv)?;
    let installed = probe_installed_version(&ctx, &python)?;
    let size = dir_size(Path::new(&row_path));
    {
        let conn = ctx
            .db
            .lock()
            .map_err(|_| "llm.db 连接锁 poisoned".to_string())?;
        env_set_ready(&conn, &ctx.env_name, &installed, size)
            .map_err(|e| format!("写库失败: {e}"))?;
    }
    task_log(&ctx, &format!("vLLM {installed} 就绪（{size} 字节）"));
    Ok(())
}

/// 任务线程公共收尾：Ok → done；Err → error + DB 环境行标 error。
fn task_thread_main(ctx: TaskCtx, kind: &str, job: impl FnOnce(TaskCtx) -> Result<(), String>) {
    match job(TaskCtx {
        task_id: ctx.task_id.clone(),
        env_name: ctx.env_name.clone(),
        envs_root: ctx.envs_root.clone(),
        uv_bin: ctx.uv_bin.clone(),
        executor: Arc::clone(&ctx.executor),
        tasks: Arc::clone(&ctx.tasks),
        db: Arc::clone(&ctx.db),
    }) {
        Ok(()) => {
            task_finish(&ctx, kind, "done", "✔ 任务完成");
        }
        Err(e) => {
            eprintln!(
                "[llm-env] 任务失败：{}（{kind} 环境 {}）：{e}",
                ctx.task_id, ctx.env_name
            );
            if let Ok(conn) = ctx.db.lock() {
                let _ = env_set_status(&conn, &ctx.env_name, "error", Some(&e));
            }
            task_finish(&ctx, kind, "error", &format!("✘ 失败：{e}"));
        }
    }
}

// ----------------------------------------------------------------------------
// REST 端点（路由 specs + 处理；由 llm.rs 的 routes()/handle() 挂载）
// ----------------------------------------------------------------------------

/// 环境管理路由 specs（handler_component=llm，与现有端点同风格；写接口 admin）。
#[must_use]
pub fn route_specs() -> Vec<RouteSpec> {
    fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: "llm".to_string(),
            requires_auth,
            required_roles: if requires_auth {
                vec!["admin".into()]
            } else {
                vec![]
            },
        }
    }
    vec![
        spec(HttpMethod::Get, "/api/v1/llm/environments", false),
        spec(HttpMethod::Post, "/api/v1/llm/environments", true),
        spec(HttpMethod::Get, "/api/v1/llm/environments/tasks", false),
        spec(HttpMethod::Get, "/api/v1/llm/environments/tasks/:id", false),
        spec(
            HttpMethod::Post,
            "/api/v1/llm/environments/:name/update",
            true,
        ),
        spec(
            HttpMethod::Post,
            "/api/v1/llm/environments/:name/default",
            true,
        ),
        spec(HttpMethod::Delete, "/api/v1/llm/environments/:name", true),
    ]
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn accepted_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 202,
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

/// 处理 `/api/v1/llm/environments*`（`segs` 为 `environments` 之后的段）。
pub fn handle(
    state: &LlmEnvState,
    method: HttpMethod,
    segs: &[&str],
    body: serde_json::Value,
) -> Result<ApiResponse, ApiGatewayError> {
    match (method, segs) {
        // —— GET /environments —— 列表 + 默认名（公开读）
        (HttpMethod::Get, []) => {
            let rows = state.rows();
            let default_name = rows.iter().find(|r| r.is_default).map(|r| r.name.clone());
            Ok(ok_json(serde_json::json!({
                "environments": rows,
                "default_name": default_name,
            })))
        }

        // —— POST /environments —— 创建 → 后台任务 → 202 {task_id}
        (HttpMethod::Post, []) => {
            let body: CreateEnvBody = serde_json::from_value(body)
                .map_err(|e| ApiGatewayError::Internal(format!("解析创建环境请求体失败: {e}")))?;
            let name = body.name.trim().to_string();
            if !valid_env_name(&name) {
                return Ok(error_response(
                    400,
                    "环境名非法（须匹配 ^[a-z0-9][a-z0-9-]{0,31}$）",
                ));
            }
            let python_version = body.python_version.unwrap_or_else(|| "3.12".into());
            if !valid_python_version(&python_version) {
                return Ok(error_response(400, "python_version 非法（形如 3.10/3.12）"));
            }
            // 渠道：缺省/空白 → stable；非法值 400
            let channel = match parse_channel(body.channel.as_deref()) {
                Ok(c) => c,
                Err(msg) => return Ok(error_response(400, &msg)),
            };
            // nightly 渠道不钉版本：vllm_version 一律视为 latest（即使带了具体
            // 版本也忽略——安装恒取 nightly 源最新轮子，注册表记 latest）
            let vllm_version = if channel == CHANNEL_NIGHTLY {
                "latest".to_string()
            } else {
                body.vllm_version.unwrap_or_else(|| "latest".into())
            };
            if !valid_vllm_version(&vllm_version) {
                return Ok(error_response(
                    400,
                    "vllm_version 非法（latest 或版本号如 0.26.0）",
                ));
            }
            let path = env_path(&state.envs_root, &name);
            // 插行（首个环境自动默认；重名 409）
            let is_default = {
                let conn = state
                    .db
                    .lock()
                    .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
                if env_get(&conn, &name).is_some() {
                    return Ok(error_response(409, &format!("推理环境已存在: {name}")));
                }
                if state.has_running_task(&name) {
                    return Ok(error_response(
                        409,
                        &format!("环境 {name} 有正在执行的任务"),
                    ));
                }
                let first = env_count(&conn) == 0;
                env_insert(&conn, &name, &path, &python_version, &vllm_version, &channel, first)
                    .map_err(|e| ApiGatewayError::Internal(format!("写库失败: {e}")))?;
                first
            };
            let task_id = state.spawn_create(
                &name,
                python_version.clone(),
                vllm_version.clone(),
                channel.clone(),
            );
            eprintln!(
                "[llm-env] 创建环境 {name}（python {python_version}，vllm {vllm_version}，channel {channel}，默认={is_default}）→ 任务 {task_id}"
            );
            Ok(accepted_json(serde_json::json!({
                "task_id": task_id,
                "env_name": name,
                "status": "creating",
                "channel": channel,
            })))
        }

        // —— GET /environments/tasks —— 任务列表（公开读；按启动时间倒序）
        (HttpMethod::Get, ["tasks"]) => {
            let tasks = state
                .tasks
                .lock()
                .map(|m| {
                    let mut v: Vec<TaskSummary> = m
                        .values()
                        .map(|t| TaskSummary {
                            id: t.id.clone(),
                            kind: t.kind.clone(),
                            env_name: t.env_name.clone(),
                            status: t.status.clone(),
                            started_at: t.started_at,
                            finished_at: t.finished_at,
                        })
                        .collect();
                    v.sort_by_key(|t| std::cmp::Reverse(t.started_at));
                    v
                })
                .unwrap_or_default();
            Ok(ok_json(serde_json::json!({ "tasks": tasks })))
        }

        // —— GET /environments/tasks/:id —— 单任务（含日志尾）
        (HttpMethod::Get, ["tasks", id]) => {
            let found = state.tasks.lock().ok().and_then(|m| m.get(*id).cloned());
            match found {
                Some(t) => Ok(ok_json(serde_json::json!({
                    "id": t.id,
                    "kind": t.kind,
                    "env_name": t.env_name,
                    "status": t.status,
                    "started_at": t.started_at,
                    "finished_at": t.finished_at,
                    "log": t.log,
                }))),
                None => Ok(error_response(404, &format!("任务不存在: {id}"))),
            }
        }

        // —— POST /environments/:name/update —— 更新 vLLM 版本/渠道 → 202
        (HttpMethod::Post, [name, "update"]) => {
            let body: UpdateEnvBody = if body.is_null() {
                UpdateEnvBody {
                    vllm_version: None,
                    channel: None,
                }
            } else {
                serde_json::from_value(body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析更新环境请求体失败: {e}"))
                })?
            };
            let name = (*name).to_string();
            // 目标渠道：请求体缺省 = 沿用该行当前渠道（nightly↔stable 可切换重装）
            let existing_channel = {
                let conn = state
                    .db
                    .lock()
                    .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
                let Some(row) = env_get(&conn, &name) else {
                    return Ok(error_response(404, &format!("推理环境不存在: {name}")));
                };
                row.channel
            };
            let channel = match parse_channel(
                body.channel
                    .as_deref()
                    .or(Some(existing_channel.as_str())),
            ) {
                Ok(c) => c,
                Err(msg) => return Ok(error_response(400, &msg)),
            };
            // nightly 渠道不钉版本：目标恒 latest（vllm_version 忽略）
            let vllm_version = if channel == CHANNEL_NIGHTLY {
                "latest".to_string()
            } else {
                body.vllm_version.unwrap_or_else(|| "latest".into())
            };
            if !valid_vllm_version(&vllm_version) {
                return Ok(error_response(
                    400,
                    "vllm_version 非法（latest 或版本号如 0.26.0）",
                ));
            }
            {
                let conn = state
                    .db
                    .lock()
                    .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
                if state.has_running_task(&name) {
                    return Ok(error_response(
                        409,
                        &format!("环境 {name} 有正在执行的任务"),
                    ));
                }
                env_set_status(&conn, &name, "updating", None)
                    .map_err(|e| ApiGatewayError::Internal(format!("写库失败: {e}")))?;
                env_set_update_target(&conn, &name, &vllm_version, &channel)
                    .map_err(|e| ApiGatewayError::Internal(format!("写库失败: {e}")))?;
            }
            let task_id = state.spawn_update(&name, vllm_version.clone(), channel.clone());
            eprintln!("[llm-env] 更新环境 {name}（vllm → {vllm_version}，channel → {channel}）→ 任务 {task_id}");
            Ok(accepted_json(serde_json::json!({
                "task_id": task_id,
                "env_name": name,
                "status": "updating",
                "channel": channel,
            })))
        }

        // —— POST /environments/:name/default —— 切换默认（事务互斥）
        (HttpMethod::Post, [name, "default"]) => {
            let name = (*name).to_string();
            let mut conn = state
                .db
                .lock()
                .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
            if env_get(&conn, &name).is_none() {
                return Ok(error_response(404, &format!("推理环境不存在: {name}")));
            }
            env_set_default(&mut conn, &name)
                .map_err(|e| ApiGatewayError::Internal(format!("切换默认失败: {e}")))?;
            drop(conn);
            eprintln!("[llm-env] 默认推理环境切换为 {name}");
            Ok(ok_json(
                serde_json::json!({"ok": true, "default_name": name}),
            ))
        }

        // —— DELETE /environments/:name —— 删行 + rm -rf venv（默认环境 409）
        (HttpMethod::Delete, [name]) => {
            let name = (*name).to_string();
            let row = {
                let conn = state
                    .db
                    .lock()
                    .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
                let Some(row) = env_get(&conn, &name) else {
                    return Ok(error_response(404, &format!("推理环境不存在: {name}")));
                };
                if row.is_default {
                    return Ok(error_response(
                        409,
                        &format!("环境 {name} 是默认环境，请先把其它环境设为默认"),
                    ));
                }
                if state.has_running_task(&name) {
                    return Ok(error_response(
                        409,
                        &format!("环境 {name} 有正在执行的任务"),
                    ));
                }
                row
            };
            // rm -rf venv（尽力而为：失败不阻塞删行，错误带回响应）
            let rm_error = std::fs::remove_dir_all(&row.path)
                .err()
                .map(|e| e.to_string());
            {
                let conn = state
                    .db
                    .lock()
                    .map_err(|_| ApiGatewayError::Internal("llm.db 连接锁 poisoned".into()))?;
                env_delete_row(&conn, &name)
                    .map_err(|e| ApiGatewayError::Internal(format!("删行失败: {e}")))?;
            }
            eprintln!(
                "[llm-env] 删除推理环境 {name}（{}）{}",
                row.path,
                rm_error
                    .as_deref()
                    .map(|e| format!("（venv 删除失败: {e}）"))
                    .unwrap_or_default()
            );
            Ok(ok_json(
                serde_json::json!({"ok": true, "name": name, "removed_path": row.path, "rm_error": rm_error}),
            ))
        }

        _ => Ok(error_response(404, "llm-env: 未匹配的路由")),
    }
}

/// `POST /api/v1/llm/environments` 请求体。
#[derive(Debug, serde::Deserialize)]
struct CreateEnvBody {
    name: String,
    #[serde(default)]
    python_version: Option<String>,
    #[serde(default)]
    vllm_version: Option<String>,
    /// 安装渠道：`stable`（缺省）| `nightly`；非法 400。
    #[serde(default)]
    channel: Option<String>,
}

/// `POST /api/v1/llm/environments/:name/update` 请求体。
#[derive(Debug, serde::Deserialize)]
struct UpdateEnvBody {
    #[serde(default)]
    vllm_version: Option<String>,
    /// 安装渠道：缺省 = 沿用该行当前渠道；`stable`|`nightly` 可切换重装。
    #[serde(default)]
    channel: Option<String>,
}

impl LlmEnvState {
    /// 登记并启动创建任务（线程体统一走 [`task_thread_main`] 收尾）。
    fn spawn_create(
        &self,
        name: &str,
        python_version: String,
        vllm_version: String,
        channel: String,
    ) -> String {
        let task_id = self.register_task("create", name);
        let ctx = self.task_ctx(&task_id, name);
        std::thread::spawn(move || {
            task_thread_main(ctx, "create", move |c| {
                run_create_task(c, python_version, vllm_version, channel)
            });
        });
        task_id
    }

    /// 登记并启动更新任务（渠道可切换：nightly↔stable 重装）。
    fn spawn_update(&self, name: &str, vllm_version: String, channel: String) -> String {
        let task_id = self.register_task("update", name);
        let ctx = self.task_ctx(&task_id, name);
        std::thread::spawn(move || {
            task_thread_main(ctx, "update", move |c| {
                run_update_task(c, vllm_version, channel)
            });
        });
        task_id
    }

    /// 登记任务（running 态）。
    fn register_task(&self, kind: &str, env_name: &str) -> String {
        let task_id = format!(
            "envtask-{}",
            self.task_seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        self.tasks.lock().expect("env tasks poisoned").insert(
            task_id.clone(),
            EnvTask {
                id: task_id.clone(),
                kind: kind.to_string(),
                env_name: env_name.to_string(),
                status: "running".into(),
                log: Vec::new(),
                started_at: now_epoch(),
                finished_at: None,
            },
        );
        task_id
    }

    /// 构造任务线程共享态。
    fn task_ctx(&self, task_id: &str, env_name: &str) -> TaskCtx {
        TaskCtx {
            task_id: task_id.to_string(),
            env_name: env_name.to_string(),
            envs_root: self.envs_root.clone(),
            uv_bin: self.uv_bin.clone(),
            executor: Arc::clone(&self.executor),
            tasks: Arc::clone(&self.tasks),
            db: Arc::clone(&self.db),
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试（mock 执行器驱动；绝不真跑 uv / 网络 / rm 生产目录）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- mock 执行器（仅测试；生产恒 UvExecutor）----

    /// 恒成功执行器：返回 (0, "ok")。
    struct AlwaysOk;

    impl EnvExecutor for AlwaysOk {
        fn run(
            &self,
            _argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            Ok((0, "ok".into()))
        }
    }

    /// 回声执行器：返回 (0, argv 拼串)——断言命令构造与日志留痕。
    struct Echo;

    impl EnvExecutor for Echo {
        fn run(&self, argv: &[&str], env_kv: &[(String, String)]) -> Result<(i32, String), String> {
            let mut out = argv.join(" ");
            for (k, v) in env_kv {
                out.push_str(&format!("\nENV {k}={v}"));
            }
            Ok((0, out))
        }
    }

    /// 命中指定参数即失败（退出码 1）的执行器——驱动 error 路径。
    struct FailOnArg(&'static str);

    impl EnvExecutor for FailOnArg {
        fn run(
            &self,
            argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            if argv.iter().any(|a| a.contains(self.0)) {
                return Ok((1, format!("boom: contains {}", self.0)));
            }
            Ok((0, "ok".into()))
        }
    }

    /// 探测返回可控版本的执行器（版本探测走 stdout 最后一个非空行）。
    struct ProbeVersion(&'static str);

    impl EnvExecutor for ProbeVersion {
        fn run(
            &self,
            argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            if argv.contains(&"-c") {
                return Ok((0, format!("warning: something\n{}", self.0)));
            }
            Ok((0, "ok".into()))
        }
    }

    // ---- 测试基建 ----

    /// 内存库 + mock 执行器 + 临时根目录 + 注入 uv 路径的状态。
    fn state_with(executor: Arc<dyn EnvExecutor>) -> LlmEnvState {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_env_schema(&conn).expect("建表必成功");
        let root = std::env::temp_dir().join(format!("nexos-llm-envs-{}", os_core::Uuid::new_v4()));
        LlmEnvState::with_executor(
            Arc::new(Mutex::new(conn)),
            root.to_string_lossy().into_owned(),
            executor,
            Some("/usr/bin/true-uv".into()), // 固定 uv 路径：不触发 PATH 扫描/自动安装
        )
    }

    /// 造一个假的可执行 uv 文件（注入路径不需要真存在——mock 不执行 argv[0]，
    /// 但 locate_uv 的固定注入分支直接返回，不查文件系统）。
    fn state_root(state: &LlmEnvState) -> String {
        state.envs_root.clone()
    }

    fn get(state: &LlmEnvState, tail: &str) -> ApiResponse {
        state.handle_req(HttpMethod::Get, tail, serde_json::Value::Null)
    }

    fn post(state: &LlmEnvState, tail: &str, body: serde_json::Value) -> ApiResponse {
        state.handle_req(HttpMethod::Post, tail, body)
    }

    fn delete(state: &LlmEnvState, tail: &str) -> ApiResponse {
        state.handle_req(HttpMethod::Delete, tail, serde_json::Value::Null)
    }

    // ---- uv 定位链测试基建（2026-09-03：多用户 glob）----

    /// env 覆盖守卫（RAII；drop 时全部 remove_var）。
    /// **只用于 NEXOS_ 前缀、仅本模块读取的 env**（如 NEXOS_LLM_UV_BIN）——
    /// PATH/HOME 是全局变量，同进程并行的 provisioning/http 测试 spawn git/ssh
    /// 也会读，进程内改写会交叉污染（定位链测试因此改为参数注入，见 locate_uv_in）。
    struct ScopedEnvs(
        Vec<&'static str>,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    impl ScopedEnvs {
        /// `pairs`：[(key, value), …]；value 传空串 = 显式压掉该 env（env_non_empty
        /// 视为未设置——测试屏蔽真机继承值）。
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let g: std::sync::MutexGuard<'static, ()> =
                ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            for (k, v) in pairs {
                std::env::set_var(k, v);
            }
            Self(pairs.iter().map(|(k, _)| *k).collect(), g)
        }
    }

    impl Drop for ScopedEnvs {
        fn drop(&mut self) {
            for k in &self.0 {
                std::env::remove_var(k);
            }
        }
    }

    /// 造一个带执行位的普通文件（0755）。
    fn write_exec(path: &std::path::Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 唯一 tempdir（tag 前缀 + 时间戳）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nexos-llm-env-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// locate_uv 测试上下文：任务已注册（日志可断言）+ 指定执行器 + uv_bin=None
    /// （走完整解析链）。
    fn locate_ctx(executor: Arc<dyn EnvExecutor>) -> (TaskCtx, Arc<Mutex<HashMap<String, EnvTask>>>) {
        let tasks: Arc<Mutex<HashMap<String, EnvTask>>> = Arc::new(Mutex::new(HashMap::new()));
        tasks.lock().unwrap().insert(
            "envtask-uv".into(),
            EnvTask {
                id: "envtask-uv".into(),
                kind: "create".into(),
                env_name: "x".into(),
                status: "running".into(),
                log: Vec::new(),
                started_at: 0,
                finished_at: None,
            },
        );
        let conn = Connection::open_in_memory().unwrap();
        let ctx = TaskCtx {
            task_id: "envtask-uv".into(),
            env_name: "x".into(),
            envs_root: "/tmp".into(),
            uv_bin: None,
            executor,
            tasks: Arc::clone(&tasks),
            db: Arc::new(Mutex::new(conn)),
        };
        (ctx, tasks)
    }

    /// 任务日志拼串（断言用）。
    fn joined_log(tasks: &Arc<Mutex<HashMap<String, EnvTask>>>) -> String {
        tasks
            .lock()
            .unwrap()
            .get("envtask-uv")
            .map(|t| t.log.join("\n"))
            .unwrap_or_default()
    }

    /// 执行 curl|sh 时在指定路径造 uv 可执行文件的 mock（驱动"自动安装成功"分支）。
    struct InstallsUv(std::path::PathBuf);

    impl EnvExecutor for InstallsUv {
        fn run(
            &self,
            argv: &[&str],
            _env_kv: &[(String, String)],
        ) -> Result<(i32, String), String> {
            if argv.iter().any(|a| a.contains("curl")) {
                write_exec(&self.0, b"#!/bin/sh\nfake-uv\n");
            }
            Ok((0, "installed".into()))
        }
    }

    #[test]
    fn scan_users_local_bin_uv_filters_executable_and_sorts() {
        // 假 /home 基座：alice 有可执行 uv；bob 的 uv 无执行位；carol 缺 .local/bin；
        // 普通文件 "afile" 非目录应跳过
        let base = temp_dir("uv-homes");
        let alice = base.join("alice/.local/bin/uv");
        let bob = base.join("bob/.local/bin/uv");
        write_exec(&alice, b"uv");
        std::fs::create_dir_all(bob.parent().unwrap()).unwrap();
        std::fs::write(&bob, b"uv").unwrap(); // 0644 无执行位
        std::fs::create_dir_all(base.join("carol/.local")).unwrap();
        std::fs::write(base.join("afile"), b"x").unwrap();

        let found = scan_users_local_bin_uv(base.to_str().unwrap());
        assert_eq!(
            found,
            vec![alice.to_string_lossy().into_owned()],
            "只应命中带执行位的 uv: {found:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_uv_env_override_wins_and_logs_path() {
        let f = temp_dir("uv-override");
        write_exec(&f.join("uv"), b"uv");
        let uv_path = f.join("uv").to_string_lossy().into_owned();
        // 只设 NEXOS_LLM_UV_BIN（本模块专属 env）；其余环节用空 PATH/空 home 注入
        let _g = ScopedEnvs::set(&[("NEXOS_LLM_UV_BIN", uv_path.as_str())]);
        let (ctx, tasks) = locate_ctx(Arc::new(AlwaysOk));
        let empty_home = temp_dir("uv-empty-home");
        let got =
            locate_uv_in(&[], empty_home.to_str().unwrap(), "/nonexistent", "/nonexistent2", &ctx)
                .unwrap();
        assert_eq!(got, uv_path);
        let log = joined_log(&tasks);
        assert!(log.contains(&uv_path), "日志应记录命中路径: {log}");
        let _ = std::fs::remove_dir_all(&f);
        let _ = std::fs::remove_dir_all(&empty_home);
    }

    #[test]
    fn locate_uv_multiuser_glob_hit_after_running_user_miss() {
        // Spark 实测形态：运行用户 home（root 语义）无 uv + uv 装在 <base>/nvidia
        let home = temp_dir("uv-home-root");
        std::fs::create_dir_all(&home).unwrap();
        let base = temp_dir("uv-home-users");
        let nvidia_uv = base.join("nvidia/.local/bin/uv");
        write_exec(&nvidia_uv, b"uv");
        let expect = nvidia_uv.to_string_lossy().into_owned();
        // 压掉真机可能存在的 NEXOS_LLM_UV_BIN 继承值（空串=未设置语义）
        let _g = ScopedEnvs::set(&[("NEXOS_LLM_UV_BIN", "")]);
        let (ctx, tasks) = locate_ctx(Arc::new(AlwaysOk));
        let root_fb = temp_dir("uv-root-fb"); // root 兜底注入为不存在路径
        let got = locate_uv_in(
            &[],
            home.to_str().unwrap(),
            base.to_str().unwrap(),
            root_fb.to_str().unwrap(),
            &ctx,
        )
        .unwrap();
        assert_eq!(got, expect, "多用户 glob 应命中 nvidia 的 uv");
        assert!(
            joined_log(&tasks).contains("多用户 ~/.local/bin 扫描找到 uv"),
            "命中环应写日志: {}",
            joined_log(&tasks)
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&root_fb);
    }

    #[test]
    fn locate_uv_running_user_home_precedes_glob() {
        // 运行用户 ~/.local/bin/uv 存在 → 优先于多用户 glob
        let home = temp_dir("uv-home-self");
        let own_uv = home.join(".local/bin/uv");
        write_exec(&own_uv, b"uv");
        let base = temp_dir("uv-home-others");
        write_exec(&base.join("nvidia/.local/bin/uv"), b"uv");
        let _g = ScopedEnvs::set(&[("NEXOS_LLM_UV_BIN", "")]);
        let (ctx, _) = locate_ctx(Arc::new(AlwaysOk));
        let got = locate_uv_in(
            &[],
            home.to_str().unwrap(),
            base.to_str().unwrap(),
            "/nonexistent-root-uv",
            &ctx,
        )
        .unwrap();
        assert_eq!(got, own_uv.to_string_lossy().into_owned());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_uv_all_miss_falls_to_auto_install() {
        // 全链未命中 → curl|sh 自动安装（mock 在 HOME/.local/bin/uv 造文件）→ 复查通过
        let home = temp_dir("uv-home-install");
        std::fs::create_dir_all(&home).unwrap();
        let base = temp_dir("uv-base-empty");
        std::fs::create_dir_all(&base).unwrap();
        let installed = home.join(".local/bin/uv");
        let _g = ScopedEnvs::set(&[("NEXOS_LLM_UV_BIN", "")]);
        let (ctx, tasks) = locate_ctx(Arc::new(InstallsUv(installed.clone())));
        let got = locate_uv_in(
            &[],
            home.to_str().unwrap(),
            base.to_str().unwrap(),
            "/nonexistent-root-uv",
            &ctx,
        )
        .unwrap();
        assert_eq!(got, installed.to_string_lossy().into_owned());
        let log = joined_log(&tasks);
        assert!(log.contains("自动安装"), "应留痕安装环: {log}");
        assert!(log.contains("uv 安装完成"), "应留痕复查通过: {log}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_uv_install_failure_propagates() {
        // 全链未命中 + 安装命令失败（mock 退出码 1）→ Err 传播
        let home = temp_dir("uv-home-fail");
        std::fs::create_dir_all(&home).unwrap();
        let base = temp_dir("uv-base-fail");
        std::fs::create_dir_all(&base).unwrap();
        let _g = ScopedEnvs::set(&[("NEXOS_LLM_UV_BIN", "")]);
        let (ctx, tasks) = locate_ctx(Arc::new(FailOnArg("curl")));
        let err = locate_uv_in(
            &[],
            home.to_str().unwrap(),
            base.to_str().unwrap(),
            "/nonexistent-root-uv",
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("退出码"), "安装失败应带退出码: {err}");
        assert!(joined_log(&tasks).contains("自动安装"));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&base);
    }

    impl LlmEnvState {
        /// 测试直调 handle（等价 llm.rs 转发后的入口；入参 tail 允许带或不带
        /// 前导 "environments" 段，与 llm.rs 的 &segs[4..] 口径对齐）。
        fn handle_req(
            &self,
            method: HttpMethod,
            tail: &str,
            body: serde_json::Value,
        ) -> ApiResponse {
            let mut segs: Vec<&str> = tail.split('/').filter(|s| !s.is_empty()).collect();
            if segs.first() == Some(&"environments") {
                segs.remove(0);
            }
            handle(self, method, &segs, body).expect("handle 不应 Err")
        }
    }

    /// 轮询任务直到非 running（mock 秒回；5s 兜底防死等）。
    fn wait_task_done(state: &LlmEnvState, task_id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let resp = get(state, &format!("/tasks/{task_id}"));
            assert_eq!(resp.status, 200, "task body: {resp:?}");
            if resp.body["status"] != "running" {
                return resp.body;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("任务 {task_id} 5s 未完成");
    }

    fn env_row(state: &LlmEnvState, name: &str) -> serde_json::Value {
        let resp = get(state, "/environments");
        assert_eq!(resp.status, 200);
        resp.body["environments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("环境 {name} 应在列表"))
    }

    fn create_env(state: &LlmEnvState, name: &str) -> String {
        let resp = post(state, "/environments", serde_json::json!({"name": name}));
        assert_eq!(resp.status, 202, "create body: {resp:?}");
        resp.body["task_id"].as_str().unwrap().to_string()
    }

    // ---- 校验纯函数 ----

    #[test]
    fn valid_env_name_accepts_and_rejects() {
        for ok in ["a", "main", "vllm-026", "e1", "0env", &"x".repeat(32)] {
            assert!(valid_env_name(ok), "{ok} 应合法");
        }
        for bad in [
            "",
            "A",
            "-lead",
            "under_score",
            "sp ace",
            "/etc",
            "../escape",
            "a#b",
            "中文",
            &"x".repeat(33),
            "a.b",
        ] {
            assert!(!valid_env_name(bad), "{bad} 应非法");
        }
    }

    #[test]
    fn valid_versions_shape() {
        assert!(valid_python_version("3.10"));
        assert!(valid_python_version("3.12"));
        assert!(valid_python_version("3.13"));
        assert!(!valid_python_version("3"));
        assert!(!valid_python_version("3.100"));
        assert!(!valid_python_version("2.7"));
        assert!(!valid_python_version("; rm -rf"));
        assert!(valid_vllm_version("latest"));
        assert!(valid_vllm_version("0.26.0"));
        assert!(valid_vllm_version("0.11.0rc1"));
        assert!(!valid_vllm_version(""));
        assert!(!valid_vllm_version("v1.0"));
        assert!(!valid_vllm_version("1.0; rm"));
    }

    #[test]
    fn vllm_spec_latest_vs_pinned() {
        assert_eq!(vllm_spec("latest"), "vllm");
        assert_eq!(vllm_spec(""), "vllm");
        assert_eq!(vllm_spec("0.26.0"), "vllm==0.26.0");
    }

    // ---- 渠道（channel）：解析 / 默认 / 非法 ----

    #[test]
    fn valid_channel_only_stable_or_nightly() {
        assert!(valid_channel("stable"));
        assert!(valid_channel("nightly"));
        for bad in ["", "Stable", "NIGHTLY", "beta", "stable ", "nightly; rm"] {
            assert!(!valid_channel(bad), "{bad:?} 应非法");
        }
    }

    #[test]
    fn parse_channel_defaults_and_rejects() {
        assert_eq!(parse_channel(None).unwrap(), "stable");
        assert_eq!(parse_channel(Some("")).unwrap(), "stable");
        assert_eq!(parse_channel(Some("  ")).unwrap(), "stable");
        assert_eq!(parse_channel(Some("stable")).unwrap(), "stable");
        assert_eq!(parse_channel(Some(" nightly ")).unwrap(), "nightly", "首尾空白应裁剪");
        assert!(parse_channel(Some("beta")).is_err());
        assert!(parse_channel(Some("Stable")).is_err(), "大小写敏感");
    }

    // ---- 安装命令构造：stable 零回归 / nightly 预置示例 ----

    #[test]
    fn pip_install_argv_stable_keeps_legacy_shape() {
        // create：无 -U（与历史命令逐字一致）
        let (argv, kv) = pip_install_argv(
            "/usr/bin/uv",
            "/r/main/bin/python",
            "0.26.0",
            "stable",
            false,
            None,
        );
        assert_eq!(
            argv,
            vec![
                "/usr/bin/uv",
                "pip",
                "install",
                "--python",
                "/r/main/bin/python",
                "vllm==0.26.0"
            ]
        );
        assert!(kv.is_empty());
        // update：-U 在 --python 前（历史顺序）
        let (argv, _) = pip_install_argv(
            "/usr/bin/uv",
            "/r/main/bin/python",
            "latest",
            "stable",
            true,
            None,
        );
        assert_eq!(
            argv,
            vec![
                "/usr/bin/uv",
                "pip",
                "install",
                "-U",
                "--python",
                "/r/main/bin/python",
                "vllm"
            ]
        );
        // stable + 镜像：argv 不带 extra-index-url，镜像走 env_kv（镜像即主源）
        let (argv, kv) = pip_install_argv(
            "uv",
            "/p",
            "0.26.0",
            "stable",
            false,
            Some("https://mirror.example/simple"),
        );
        assert!(!argv.iter().any(|a| a == "--extra-index-url"));
        assert_eq!(
            kv,
            vec![
                ("UV_PIP_INDEX_URL".to_string(), "https://mirror.example/simple".to_string()),
                ("PIP_INDEX_URL".to_string(), "https://mirror.example/simple".to_string()),
            ]
        );
    }

    #[test]
    fn pip_install_argv_nightly_matches_preset_example() {
        // 用户点名示例：uv pip install -U vllm --torch-backend=auto
        //   --extra-index-url https://wheels.vllm.ai/nightly
        // （实现按环境加 --python <env>/bin/python；版本参数恒被忽略）
        let (argv, kv) = pip_install_argv(
            "/usr/bin/uv",
            "/r/night/bin/python",
            "0.26.0",
            "nightly",
            false,
            None,
        );
        assert_eq!(
            argv,
            vec![
                "/usr/bin/uv",
                "pip",
                "install",
                "--python",
                "/r/night/bin/python",
                "-U",
                "vllm",
                "--torch-backend=auto",
                "--extra-index-url",
                VLLM_NIGHTLY_INDEX,
            ]
        );
        assert!(kv.is_empty(), "nightly 不走 UV_PIP_INDEX_URL env（避免镜像顶成主源）");
        assert!(!argv.iter().any(|a| a.starts_with("vllm==")), "nightly 不钉版本");
        // 镜像叠加：nightly 源在前（主），镜像在后（PyPI 兜底）——顺序即优先级
        let (argv, kv) = pip_install_argv(
            "uv",
            "/p",
            "latest",
            "nightly",
            true,
            Some("https://mirror.example/simple"),
        );
        let i_nightly = argv.iter().position(|a| a == VLLM_NIGHTLY_INDEX).unwrap();
        let i_mirror = argv
            .iter()
            .position(|a| a == "https://mirror.example/simple")
            .unwrap();
        assert_eq!(argv[i_nightly - 1], "--extra-index-url");
        assert_eq!(argv[i_mirror - 1], "--extra-index-url");
        assert!(i_nightly < i_mirror, "nightly 源必须在前（主源）");
        assert!(kv.is_empty());
    }

    #[test]
    fn tail_chars_keeps_suffix() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 10), "ab");
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn route_specs_declare_seven_env_endpoints_with_admin_writes() {
        let specs = route_specs();
        assert_eq!(specs.len(), 7, "应有 7 条环境路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "llm"));
        for s in &specs {
            if s.method == HttpMethod::Get {
                assert!(!s.requires_auth, "GET 应公开: {s:?}");
            } else {
                assert!(s.requires_auth, "写操作需 auth: {s:?}");
                assert_eq!(s.required_roles, vec!["admin".to_string()]);
            }
        }
        for p in [
            "/api/v1/llm/environments",
            "/api/v1/llm/environments/tasks",
            "/api/v1/llm/environments/tasks/:id",
            "/api/v1/llm/environments/:name/update",
            "/api/v1/llm/environments/:name/default",
            "/api/v1/llm/environments/:name",
        ] {
            assert!(specs.iter().any(|s| s.path == p), "缺路由 {p}");
        }
    }

    // ---- 列表 / 创建状态机 ----

    #[tokio::test]
    async fn list_empty_returns_empty_and_null_default() {
        let state = state_with(Arc::new(AlwaysOk));
        let resp = get(&state, "/environments");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["environments"].as_array().unwrap().len(), 0);
        assert!(resp.body["default_name"].is_null());
    }

    #[tokio::test]
    async fn create_task_transitions_creating_to_ready_and_first_is_default() {
        let state = state_with(Arc::new(ProbeVersion("0.26.0")));
        let root = state_root(&state);
        // 预放一个文件验 size 递归求和（venv 目录树由 uv 真建，mock 不建目录——
        // size 断言只看 ≥0；这里至少验证 ready + 版本记录）
        let task_id = create_env(&state, "main");
        let task = wait_task_done(&state, &task_id);
        assert_eq!(task["status"], "done", "任务日志: {task:?}");
        assert_eq!(task["kind"], "create");
        // 日志含命令留痕（uv venv / pip install）
        let log = task["log"].as_array().unwrap();
        let joined = log
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("venv --python 3.12"),
            "日志缺 venv 命令: {joined}"
        );
        assert!(
            joined.contains("pip install --python"),
            "日志缺 pip 命令: {joined}"
        );
        // 环境行 ready + 首个自动默认 + 版本探测值
        let row = env_row(&state, "main");
        assert_eq!(row["status"], "ready");
        assert_eq!(row["is_default"], true, "首个环境应自动设为默认");
        assert_eq!(row["vllm_version_requested"], "latest");
        assert_eq!(row["vllm_version_installed"], "0.26.0");
        assert!(row["size_bytes"].as_u64().is_some());
        // 列表 default_name
        let resp = get(&state, "/environments");
        assert_eq!(resp.body["default_name"], "main");
        // 清理临时目录
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn create_failure_marks_error_with_last_error() {
        let state = state_with(Arc::new(FailOnArg("venv"))); // uv venv 步骤即失败
        let task_id = create_env(&state, "bad");
        let task = wait_task_done(&state, &task_id);
        assert_eq!(task["status"], "error");
        let row = env_row(&state, "bad");
        assert_eq!(row["status"], "error");
        assert!(
            row["last_error"]
                .as_str()
                .unwrap_or("")
                .contains("退出码 1"),
            "last_error 应含退出码: {row:?}"
        );
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn create_rejects_invalid_names_and_duplicate() {
        let state = state_with(Arc::new(AlwaysOk));
        // 路径穿越/非法名 400
        for bad in ["../escape", "UPPER", "-lead", "a_b", ""] {
            let resp = post(&state, "/environments", serde_json::json!({"name": bad}));
            assert_eq!(resp.status, 400, "name={bad:?} 应 400");
        }
        // 非法版本 400
        let resp = post(
            &state,
            "/environments",
            serde_json::json!({"name": "ok1", "vllm_version": "1.0; rm -rf"}),
        );
        assert_eq!(resp.status, 400);
        let resp = post(
            &state,
            "/environments",
            serde_json::json!({"name": "ok1", "python_version": "2.7"}),
        );
        assert_eq!(resp.status, 400);
        // 合法创建后重名 409
        let task = create_env(&state, "dup");
        wait_task_done(&state, &task);
        let resp = post(&state, "/environments", serde_json::json!({"name": "dup"}));
        assert_eq!(resp.status, 409, "重名应 409: {resp:?}");
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    // ---- 更新 / 默认 / 删除 ----

    #[tokio::test]
    async fn update_task_reinstalls_and_records_new_version() {
        let state = state_with(Arc::new(ProbeVersion("0.27.1")));
        let t1 = create_env(&state, "main");
        wait_task_done(&state, &t1);
        // 更新到指定版本
        let resp = post(
            &state,
            "/environments/main/update",
            serde_json::json!({"vllm_version": "0.27.0"}),
        );
        assert_eq!(resp.status, 202, "update body: {resp:?}");
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task_done(&state, &task_id);
        assert_eq!(task["kind"], "update");
        let log_joined = task["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            log_joined.contains("pip install -U --python"),
            "更新应带 -U: {log_joined}"
        );
        let row = env_row(&state, "main");
        assert_eq!(row["status"], "ready");
        assert_eq!(row["vllm_version_requested"], "0.27.0");
        assert_eq!(row["vllm_version_installed"], "0.27.1");
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn update_missing_env_returns_404_and_bad_version_400() {
        let state = state_with(Arc::new(AlwaysOk));
        let resp = post(
            &state,
            "/environments/nope/update",
            serde_json::json!({"vllm_version": "latest"}),
        );
        assert_eq!(resp.status, 404);
        let t = create_env(&state, "main");
        wait_task_done(&state, &t);
        let resp = post(
            &state,
            "/environments/main/update",
            serde_json::json!({"vllm_version": "zzz"}),
        );
        assert_eq!(resp.status, 400);
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    // ---- 渠道（channel）：默认 stable / 非法 400 / nightly 全流转 / 切换 ----

    #[tokio::test]
    async fn channel_defaults_stable_and_rejects_invalid() {
        let state = state_with(Arc::new(AlwaysOk));
        // 缺省 / 空串 → stable（现状零变化）
        let t = create_env(&state, "dflt");
        wait_task_done(&state, &t);
        assert_eq!(env_row(&state, "dflt")["channel"], "stable");
        let resp = post(
            &state,
            "/environments",
            serde_json::json!({"name": "blank", "channel": ""}),
        );
        assert_eq!(resp.status, 202, "空渠道应视作缺省: {resp:?}");
        assert_eq!(resp.body["channel"], "stable");
        wait_task_done(&state, resp.body["task_id"].as_str().unwrap());
        // 非法渠道 400（create 与 update 双端点）
        for bad in ["beta", "Stable", "nightly; rm"] {
            let resp = post(
                &state,
                "/environments",
                serde_json::json!({"name": "x1", "channel": bad}),
            );
            assert_eq!(resp.status, 400, "channel={bad:?} 应 400");
        }
        let resp = post(
            &state,
            "/environments/dflt/update",
            serde_json::json!({"channel": "beta"}),
        );
        assert_eq!(resp.status, 400, "update 非法渠道应 400: {resp:?}");
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn nightly_create_runs_preset_command_and_marks_channel() {
        // Echo 回显 argv → 任务日志即命令行断言（注入 executor 捕获 argv 手法）
        let state = state_with(Arc::new(Echo));
        let resp = post(
            &state,
            "/environments",
            serde_json::json!({
                "name": "night",
                "channel": "nightly",
                "vllm_version": "0.26.0" // nightly 恒最新：版本参数应被忽略
            }),
        );
        assert_eq!(resp.status, 202, "create body: {resp:?}");
        assert_eq!(resp.body["channel"], "nightly");
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task_done(&state, &task_id);
        assert_eq!(task["status"], "done", "任务日志: {task:?}");
        let joined = task["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        // 完整预置命令进日志（含 --torch-backend=auto 与 nightly extra-index-url）
        assert!(
            joined.contains(
                "pip install --python",
            ),
            "缺 pip install: {joined}"
        );
        assert!(
            joined.contains("-U vllm --torch-backend=auto --extra-index-url https://wheels.vllm.ai/nightly"),
            "nightly 命令应含 torch-backend 与 nightly 源: {joined}"
        );
        assert!(!joined.contains("vllm=="), "nightly 不钉版本: {joined}");
        // 注册表：channel=nightly + 请求版本规范化为 latest
        let row = env_row(&state, "night");
        assert_eq!(row["channel"], "nightly");
        assert_eq!(row["vllm_version_requested"], "latest");
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn update_switches_channel_nightly_and_back() {
        let state = state_with(Arc::new(Echo));
        // ① stable 创建（默认渠道）
        let t1 = create_env(&state, "main");
        wait_task_done(&state, &t1);
        assert_eq!(env_row(&state, "main")["channel"], "stable");
        // ② 切 nightly：202 带 channel、命令换 nightly 形态、行字段更新
        let resp = post(
            &state,
            "/environments/main/update",
            serde_json::json!({"channel": "nightly"}),
        );
        assert_eq!(resp.status, 202, "update body: {resp:?}");
        assert_eq!(resp.body["channel"], "nightly");
        let t2 = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task_done(&state, &t2);
        let joined = task["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("-U vllm --torch-backend=auto --extra-index-url https://wheels.vllm.ai/nightly"),
            "切换 nightly 后命令应带新参数: {joined}"
        );
        let row = env_row(&state, "main");
        assert_eq!(row["channel"], "nightly");
        assert_eq!(row["vllm_version_requested"], "latest");
        // ③ 缺省渠道沿用当前值（不回退 stable）
        let resp = post(
            &state,
            "/environments/main/update",
            serde_json::json!({ "vllm_version": "latest" }),
        );
        assert_eq!(resp.status, 202);
        wait_task_done(&state, resp.body["task_id"].as_str().unwrap());
        assert_eq!(
            env_row(&state, "main")["channel"],
            "nightly",
            "update 缺省渠道应沿用当前行值"
        );
        // ④ 切回 stable：命令恢复钉版本形态、无 nightly 参数
        let resp = post(
            &state,
            "/environments/main/update",
            serde_json::json!({"channel": "stable", "vllm_version": "0.26.0"}),
        );
        assert_eq!(resp.status, 202);
        let t4 = resp.body["task_id"].as_str().unwrap().to_string();
        let task = wait_task_done(&state, &t4);
        let joined = task["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("pip install -U --python") && joined.contains("vllm==0.26.0"),
            "切回 stable 应恢复钉版本命令: {joined}"
        );
        assert!(!joined.contains("wheels.vllm.ai"), "stable 命令不应带 nightly 源: {joined}");
        let row = env_row(&state, "main");
        assert_eq!(row["channel"], "stable");
        assert_eq!(row["vllm_version_requested"], "0.26.0");
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn set_default_is_exclusive() {
        let state = state_with(Arc::new(AlwaysOk));
        for n in ["main", "second"] {
            let t = create_env(&state, n);
            wait_task_done(&state, &t);
        }
        assert_eq!(env_row(&state, "main")["is_default"], true);
        // 切默认 → second 唯一默认
        let resp = post(
            &state,
            "/environments/second/default",
            serde_json::Value::Null,
        );
        assert_eq!(resp.status, 200, "default body: {resp:?}");
        assert_eq!(resp.body["default_name"], "second");
        assert_eq!(env_row(&state, "second")["is_default"], true);
        assert_eq!(
            env_row(&state, "main")["is_default"],
            false,
            "旧默认应清除（互斥）"
        );
        // 不存在的环境 404
        let resp = post(
            &state,
            "/environments/nope/default",
            serde_json::Value::Null,
        );
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn delete_removes_row_and_dir_but_default_rejected() {
        let state = state_with(Arc::new(AlwaysOk));
        for n in ["main", "spare"] {
            let t = create_env(&state, n);
            wait_task_done(&state, &t);
        }
        // 默认环境拒删（409）
        let resp = delete(&state, "/environments/main");
        assert_eq!(resp.status, 409, "默认环境删除应 409: {resp:?}");
        // 非默认：删行 + rm -rf 目录（先放个文件验目录真被删）
        let dir = format!("{}/spare", state_root(&state));
        std::fs::create_dir_all(format!("{dir}/bin")).unwrap();
        std::fs::write(format!("{dir}/bin/vllm"), b"fake").unwrap();
        let resp = delete(&state, "/environments/spare");
        assert_eq!(resp.status, 200, "delete body: {resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert!(!std::path::Path::new(&dir).exists(), "venv 目录应被删除");
        let resp = get(&state, "/environments");
        assert!(
            !resp.body["environments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "spare"),
            "行应被删"
        );
        // 再删已不存在的 404
        let resp = delete(&state, "/environments/spare");
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    // ---- 任务视图 ----

    #[tokio::test]
    async fn tasks_list_and_detail_with_log() {
        let state = state_with(Arc::new(Echo));
        let t1 = create_env(&state, "main");
        wait_task_done(&state, &t1);
        let resp = get(&state, "/tasks");
        assert_eq!(resp.status, 200);
        let arr = resp.body["tasks"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"], "done");
        assert!(arr[0].get("log").is_none(), "列表不应带日志");
        // 详情带日志 + Echo 断言命令构造（spec=latest → vllm 裸名）
        let resp = get(&state, &format!("/tasks/{t1}"));
        assert_eq!(resp.status, 200);
        let log = resp.body["log"].as_array().unwrap();
        let joined = log
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("/usr/bin/true-uv venv --python 3.12"),
            "日志缺 venv: {joined}"
        );
        assert!(
            joined.contains("pip install --python"),
            "日志缺 install: {joined}"
        );
        assert!(joined.contains(" vllm"), "latest 应装裸 vllm: {joined}");
        assert!(
            joined.contains("importlib.metadata"),
            "日志缺版本探测: {joined}"
        );
        // 不存在的任务 404
        let resp = get(&state, "/tasks/nope");
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(state_root(&state));
    }

    #[tokio::test]
    async fn task_log_ring_buffer_caps_at_200_lines() {
        // 直接压 300 行日志验证环形截断
        let conn = Connection::open_in_memory().unwrap();
        create_env_schema(&conn).unwrap();
        let tasks: Arc<Mutex<HashMap<String, EnvTask>>> = Arc::new(Mutex::new(HashMap::new()));
        tasks.lock().unwrap().insert(
            "envtask-1".into(),
            EnvTask {
                id: "envtask-1".into(),
                kind: "create".into(),
                env_name: "x".into(),
                status: "running".into(),
                log: Vec::new(),
                started_at: 0,
                finished_at: None,
            },
        );
        let ctx = TaskCtx {
            task_id: "envtask-1".into(),
            env_name: "x".into(),
            envs_root: "/tmp".into(),
            uv_bin: None,
            executor: Arc::new(AlwaysOk),
            tasks: Arc::clone(&tasks),
            db: Arc::new(Mutex::new(conn)),
        };
        for i in 0..300 {
            task_log(&ctx, &format!("line-{i}"));
        }
        let log = tasks.lock().unwrap().get("envtask-1").unwrap().log.clone();
        assert_eq!(log.len(), TASK_LOG_MAX_LINES);
        assert_eq!(log.first().unwrap(), "line-100", "应保留最后 200 行");
        assert_eq!(log.last().unwrap(), "line-299");
    }

    // ---- 持久化辅助 ----

    #[test]
    fn env_roundtrip_and_default_ready_lookup() {
        let conn = Connection::open_in_memory().unwrap();
        create_env_schema(&conn).unwrap();
        env_insert(&conn, "a", "/tmp/a", "3.12", "latest", "stable", true).unwrap();
        env_insert(&conn, "b", "/tmp/b", "3.11", "0.26.0", "nightly", false).unwrap();
        let rows = env_list(&conn);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a"); // 按 id（创建序）
        assert!(rows[0].is_default);
        assert_eq!(rows[0].status, "creating");
        assert_eq!(rows[0].channel, "stable");
        assert_eq!(rows[1].channel, "nightly");
        // 非 ready 的默认行不算「默认就绪」
        assert!(default_ready_env(&conn).is_none());
        env_set_ready(&conn, "a", "0.26.0", 1024).unwrap();
        let d = default_ready_env(&conn).expect("ready 默认行应命中");
        assert_eq!(d.name, "a");
        assert_eq!(d.vllm_version_installed.as_deref(), Some("0.26.0"));
        assert_eq!(d.size_bytes, 1024);
        // 状态置错 + last_error
        env_set_status(&conn, "a", "error", Some("boom")).unwrap();
        assert_eq!(
            env_get(&conn, "a").unwrap().last_error.as_deref(),
            Some("boom")
        );
        // update 目标落库（版本 + 渠道切换）
        env_set_update_target(&conn, "a", "0.27.0", "nightly").unwrap();
        let a = env_get(&conn, "a").unwrap();
        assert_eq!(a.vllm_version_requested.as_deref(), Some("0.27.0"));
        assert_eq!(a.channel, "nightly");
        // 切默认互斥
        let mut conn2 = conn;
        env_set_default(&mut conn2, "b").unwrap();
        let rows = env_list(&conn2);
        assert!(!rows[0].is_default && rows[1].is_default);
        // 删行
        env_delete_row(&conn2, "b").unwrap();
        assert!(env_get(&conn2, "b").is_none());
    }

    #[test]
    fn legacy_rows_without_channel_read_as_stable() {
        // 2026-09-02 前的存量表无 channel 列；ALTER 迁移补列后旧行 NULL →
        // 读取兜底 stable（不误判 nightly、不炸反序列化）
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE llm_environments (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL,
                path TEXT NOT NULL, python_version TEXT, vllm_version_requested TEXT,
                vllm_version_installed TEXT, is_default INTEGER DEFAULT 0,
                status TEXT DEFAULT 'creating', created_at INTEGER, updated_at INTEGER,
                last_error TEXT, size_bytes INTEGER DEFAULT 0
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO llm_environments
             (name,path,python_version,vllm_version_requested,vllm_version_installed,
              is_default,status,created_at,updated_at,size_bytes)
             VALUES ('legacy','/tmp/legacy','3.12','latest','0.25.0',1,'ready',1,1,0)",
            [],
        )
        .unwrap();
        create_env_schema(&conn).unwrap(); // 幂等：补 channel 列
        let rows = env_list(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel, "stable", "存量行 NULL 应读作 stable");
    }

    #[test]
    fn dir_size_sums_recursively() {
        let dir =
            std::env::temp_dir().join(format!("nexos-llm-env-size-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/vllm"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("pyvenv.cfg"), vec![0u8; 28]).unwrap();
        assert_eq!(dir_size(&dir), 128);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(dir_size(&dir), 0, "不存在返回 0");
    }

    #[test]
    fn uv_executor_timeout_for_by_command_kind() {
        assert_eq!(
            UvExecutor::timeout_for(&["uv", "venv", "--python", "3.12"]),
            UV_VENV_TIMEOUT
        );
        assert_eq!(
            UvExecutor::timeout_for(&["uv", "pip", "install", "vllm"]),
            UV_PIP_TIMEOUT
        );
        assert_eq!(
            UvExecutor::timeout_for(&["python", "-c", "print(1)"]),
            PROBE_TIMEOUT
        );
        assert_eq!(
            UvExecutor::timeout_for(&["sh", "-c", "curl x | sh"]),
            UV_SELF_INSTALL_TIMEOUT
        );
    }

    // 未覆盖路由兜底
    #[tokio::test]
    async fn unmatched_env_route_returns_404() {
        let state = state_with(Arc::new(AlwaysOk));
        let resp = state.handle_req(HttpMethod::Put, "/environments", serde_json::Value::Null);
        assert_eq!(resp.status, 404);
    }
}
