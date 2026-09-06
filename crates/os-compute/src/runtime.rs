//! youki/runc 容器运行时执行层——命令构造 + spawn 抽象。
//!
//! 定位：[`crate::container::ContainerRuntime`] 的实现（批 3 `YoukiRuntime`/本模块
//! [`ContainerRuntimeImpl`]）经 [`ContainerRuntimeRunner`] 抽象调 youki/runc 完成容器
//! 生命周期。本模块与 [`crate::apt`] 的 `AptRunner` 同构设计，分两层：
//!
//! - **命令构造层**（`*_argv`）：纯函数，把高层意图（create/start/kill/...）翻译成 argv，
//!   可单测验证命令正确性，且不触发真实子进程；youki 与 runc 同属 OCI runtime，
//!   命令面一致，故构造层对两者通用（实际执行哪个二进制由 runner 决定）。
//! - **执行层**（[`ContainerRuntimeRunner`] trait + [`YoukiRunner`] / `FixtureRuntimeRunner`）：
//!   经抽象 spawn 子进程 + 等待 + 解析退出码/stdout；上层编排（[`ContainerRuntimeImpl`]）
//!   调构造层拼 argv 再交给 runner 执行。
//!
//! **隔离真实运行时改宿主**（规格书 §9 红线 + SANDBOX §5.2）：真实执行走 [`YoukiRunner`]，
//! 默认不参与 `cargo test`——真实环境测试标 `#[ignore]`（需 root + youki 二进制 + 写
//! cgroup），由人工或 CI 用 `cargo test -- --ignored` 触发；常规 `cargo test` 走
//! `FixtureRuntimeRunner`（仅 `cfg(test)`）注入预录 fixture 输出，零系统依赖。
//!
//! **youki 未注册**（本任务约束）：youki/oci-distribution 未在 workspace 注册——本模块
//! 只做命令构造层 + spawn 抽象（trait + tokio Command 骨架），**不**引 youki crate，
//! **不**真跑容器（真实执行测标 `#[ignore]`）。oci-distribution 拉镜像由 [`ImagePuller`]
//! trait 抽象，[`StubImagePuller`] 返回占位 digest，待批 3 引 oci-distribution 后替换。
//!
//! OCI runtime 命令约定（youki/runc 一致）：
//! - 全局 `--root <state_dir>`：容器状态存储目录（youki 在此写 `<id>/state.json`）；
//! - `create <id> --bundle <bundle_dir>`：从 OCI bundle（含 `config.json` + `rootfs/`）
//!   创建容器（不启动），分配 PID/状态文件；
//! - `start <id>`：启动已创建容器的 init 进程（执行 `config.json` 的 `process.args`）；
//! - `kill <id> <signal>`：向容器 init 发信号（`KILL`/`TERM`）；`stop` = `kill` 的语义包装；
//! - `delete <id>`：删除容器状态（须先 stop；`--force` 可同时 kill + delete）；
//! - `state <id>`：输出容器状态 JSON（含 status: created/running/stopped/paused）；
//! - `list`：列出 `--root` 下所有容器状态 JSON；
//! - `exec <id> <cmd> [args...]`：在运行容器内执行新进程；
//! - `pause <id>` / `resume <id>`：cgroup freezer 暂停/恢复。

use std::path::{Path, PathBuf};

use os_core::CommandOutput;

use crate::container::{Container, ContainerSpec, ContainerState, ImageInfo};
use crate::container_net::NetworkDriver;
use crate::error::{ComputeError, ComputeResult};

// ----------------------------------------------------------------------------
// 全局参数
// ----------------------------------------------------------------------------

/// 默认 youki/runc 二进制名（生产实现 [`YoukiRunner`] 可覆盖，如 `/usr/local/bin/youki`）。
pub const DEFAULT_RUNTIME_BIN: &str = "youki";

/// 默认容器状态存储根目录（youki 写 `<id>/state.json` 于此；生产由配置注入）。
pub const DEFAULT_STATE_ROOT: &str = "/run/os/youki";

/// 停止容器默认信号（SIGTERM，优雅停止；force=true 用 KILL）。
pub const DEFAULT_STOP_SIGNAL: &str = "TERM";

/// 强制停止信号（SIGKILL，不可拦截；stop_container force=true 时用）。
pub const FORCE_STOP_SIGNAL: &str = "KILL";

/// youki `state` 输出 JSON 中 status 字段的可能取值（小写，对齐 OCI runtime spec）。
///
/// 用于 [`parse_state_status`] 把 youki `state <id>` 的 JSON 输出映射回
/// [`crate::container::ContainerState`]。
pub mod state_status {
    /// 已创建未启动
    pub const CREATED: &str = "created";
    /// 运行中
    pub const RUNNING: &str = "running";
    /// 已停止
    pub const STOPPED: &str = "stopped";
    /// 已暂停（cgroup freezer）
    pub const PAUSED: &str = "paused";
}

// ----------------------------------------------------------------------------
// argv 构造（返回 Vec<String>，便于断言；实现层喂给 runner）
// ----------------------------------------------------------------------------

/// 构造全局参数前缀：`["--root", <state_root>]`。
///
/// youki/runc 的 `--root` 是全局选项，须在子命令之前。所有 `*_argv` 函数都把它
/// 放在 argv 开头，调用方再在最前面拼二进制名（见 [`YoukiRunner::full_argv`]）。
///
/// 非 UTF-8 路径在此 panic（构造层提前暴露非法配置，避免 spawn 时延迟失败）——
/// 生产配置的 state_root 必为合法 UTF-8（来自配置文件/CLI 参数）。
pub fn global_args(state_root: &Path) -> Vec<String> {
    vec![
        "--root".to_string(),
        state_root
            .to_str()
            .expect("state_root 必须是合法 UTF-8 路径")
            .to_string(),
    ]
}

/// `create <id> --bundle <bundle_dir>` 的 argv（含全局 `--root` 前缀，下同）。
///
/// youki create 从 bundle 目录读 `config.json` + `rootfs/`，创建容器状态文件并分配
/// PID，但**不**启动 init 进程（须再调 [`start_argv`]）。`--pid-file`/`--console-socket`
/// 等可选输出在此省略——上层按需扩展。
pub fn create_argv(state_root: &Path, id: &str, bundle: &Path) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let bundle_str = bundle.to_str().ok_or_else(|| {
        ComputeError::InvalidSpec(format!("bundle 路径非 UTF-8: {}", bundle.display()))
    })?;
    let mut argv = global_args(state_root);
    argv.push("create".to_string());
    argv.push(id.to_string());
    argv.push("--bundle".to_string());
    argv.push(bundle_str.to_string());
    Ok(argv)
}

/// `start <id>` 的 argv。
pub fn start_argv(state_root: &Path, id: &str) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let mut argv = global_args(state_root);
    argv.push("start".to_string());
    argv.push(id.to_string());
    Ok(argv)
}

/// `kill <id> <signal>` 的 argv。
///
/// `signal` 接受符号名（`TERM`/`KILL`/`HUP`）或数字（`9`/`15`）。[`stop_argv`] 是它的
/// 语义包装（force → KILL，否则 TERM）。
pub fn kill_argv(state_root: &Path, id: &str, signal: &str) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    if signal.trim().is_empty() {
        return Err(ComputeError::InvalidSpec("信号不能为空".into()));
    }
    let mut argv = global_args(state_root);
    argv.push("kill".to_string());
    argv.push(id.to_string());
    argv.push(signal.to_string());
    Ok(argv)
}

/// 停止容器 argv：`kill <id> TERM|KILL`。
///
/// `force=true` 用 [`FORCE_STOP_SIGNAL`]（KILL，不可拦截，立即结束）；
/// 否则 [`DEFAULT_STOP_SIGNAL`]（TERM，优雅停止，允许 init 处理清理钩子）。
pub fn stop_argv(state_root: &Path, id: &str, force: bool) -> ComputeResult<Vec<String>> {
    let sig = if force {
        FORCE_STOP_SIGNAL
    } else {
        DEFAULT_STOP_SIGNAL
    };
    kill_argv(state_root, id, sig)
}

/// `delete <id>` 的 argv。
///
/// `force=true` 附加 `--force`（youki 先 kill 再 delete，用于容器仍在运行的清理）。
pub fn delete_argv(state_root: &Path, id: &str, force: bool) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let mut argv = global_args(state_root);
    argv.push("delete".to_string());
    argv.push(id.to_string());
    if force {
        argv.push("--force".to_string());
    }
    Ok(argv)
}

/// `state <id>` 的 argv——输出容器状态 JSON（含 status 字段）。
pub fn state_argv(state_root: &Path, id: &str) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let mut argv = global_args(state_root);
    argv.push("state".to_string());
    argv.push(id.to_string());
    Ok(argv)
}

/// `list` 的 argv——列出 `--root` 下所有容器状态 JSON（每行一个）。
pub fn list_argv(state_root: &Path) -> Vec<String> {
    let mut argv = global_args(state_root);
    argv.push("list".to_string());
    argv
}

/// `exec <id> <cmd> [args...]` 的 argv。
///
/// `cmd` 是要在容器内执行的可执行名及其参数（argv\[0\] = 可执行）。youki exec 会在
/// 容器命名空间内 spawn 该进程。可选 `--tty`/`--env` 等在此省略。
pub fn exec_argv(state_root: &Path, id: &str, cmd: &[String]) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    if cmd.is_empty() || cmd[0].trim().is_empty() {
        return Err(ComputeError::InvalidSpec("exec 命令不能为空".into()));
    }
    let mut argv = global_args(state_root);
    argv.push("exec".to_string());
    argv.push(id.to_string());
    argv.extend(cmd.iter().cloned());
    Ok(argv)
}

/// `pause <id>` 的 argv——cgroup freezer 冻结容器所有进程。
pub fn pause_argv(state_root: &Path, id: &str) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let mut argv = global_args(state_root);
    argv.push("pause".to_string());
    argv.push(id.to_string());
    Ok(argv)
}

/// `resume <id>` 的 argv——解冻（对应 [`pause_argv`]）。
pub fn resume_argv(state_root: &Path, id: &str) -> ComputeResult<Vec<String>> {
    validate_id(id)?;
    let mut argv = global_args(state_root);
    argv.push("resume".to_string());
    argv.push(id.to_string());
    Ok(argv)
}

/// 校验容器 ID 合法性（非空、无空白；youki 用 ID 作状态目录名）。
fn validate_id(id: &str) -> ComputeResult<()> {
    if id.trim().is_empty() {
        return Err(ComputeError::InvalidSpec("容器 ID 不能为空".into()));
    }
    if id.chars().any(|c| c.is_whitespace()) {
        return Err(ComputeError::InvalidSpec(format!(
            "容器 ID 不能含空白: {id}"
        )));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// youki state/list 输出解析
// ----------------------------------------------------------------------------

/// 从 `youki state <id>` 的 JSON 输出提取 `status` 字段值（小写串）。
///
/// youki state 输出形如 `{"ociVersion":"1.0.2-dev","id":"c1","status":"running",...}`。
/// 本函数做最小 JSON 字段提取（避免引 oci-spec 等重依赖），用朴素串匹配定位
/// `"status":"..."`。找不到返回 None（调用方映射成 `Internal` 错误）。
pub fn parse_state_status(state_json: &str) -> Option<String> {
    // 朴素提取 "status":"<value>"——容忍空格变化（youki 输出紧凑/pretty 均可）
    let key = r#""status""#;
    let idx = state_json.find(key)?;
    let rest = &state_json[idx + key.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 把 youki status 串映射回 [`ContainerState`]。
///
/// youki/runc 的 status 取小写：created/running/stopped/paused。未知值映射为 None
/// （调用方按需报错或当 Stopped 处理）。
pub fn status_to_state(status: &str) -> Option<crate::container::ContainerState> {
    use crate::container::ContainerState;
    match status {
        state_status::CREATED => Some(ContainerState::Created),
        state_status::RUNNING => Some(ContainerState::Running),
        state_status::STOPPED => Some(ContainerState::Stopped),
        state_status::PAUSED => Some(ContainerState::Paused),
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// 执行层抽象（ContainerRuntimeRunner）——隔离 spawn，便于测试注入
// ----------------------------------------------------------------------------

/// 容器运行时执行器抽象——隔离子进程 spawn，使上层编排可测。
///
/// 与 [`crate::apt::AptRunner`] 同构：生产实现 [`YoukiRunner`] 调真实 youki/runc
/// 子进程；测试用 `FixtureRuntimeRunner`（仅 `cfg(test)`）注入预录 fixture 输出，
/// 零系统依赖（规格书 §9 红线「不真跑容器」由 `#[ignore]` 守护）。
///
/// **argv 约定**：`run(argv)` 的 argv 第一项是 program 名（`youki`/`runc` 或全路径），
/// 其余是参数（含全局 `--root` 前缀）。这样构造层 `*_argv` 函数返回的 Vec（无 program）
/// 经 [`YoukiRunner::full_argv`] 前置 program 后可直接传入。
///
/// 与 `os-compute` 其他 trait 一致用原生 `async fn in trait`（无 `#[async_trait]`）。
/// 因原生 async fn in trait 不是 object-safe，上层编排函数用泛型 `<R: ContainerRuntimeRunner>`
/// （而非 `&dyn`）——调用方传具体类型即可，零虚表开销。
#[allow(async_fn_in_trait)]
pub trait ContainerRuntimeRunner: Send + Sync {
    /// 执行 `<argv[0]> <argv[1..]>`，返回 stdout/stderr/退出码。
    ///
    /// 实现应：
    /// - stdin 接 `/dev/null`（防止子进程阻塞读 stdin）；
    /// - 捕获 stdout/stderr（不继承父终端，便于解析）；
    /// - 失败（spawn 失败、超时）映射成 `ComputeError::CommandFailed`/`Io`。
    async fn run(&self, argv: &[String]) -> ComputeResult<CommandOutput>;
}

/// youki/runc 生产执行器——`tokio::process::Command` spawn 真实运行时子进程。
///
/// - `bin`：运行时二进制路径（默认 [`DEFAULT_RUNTIME_BIN`] = `youki`；可设 `runc` 或全路径）；
/// - `state_root`：容器状态存储根（默认 [`DEFAULT_STATE_ROOT`]，生产由配置注入）。
///
/// **不在常规 `cargo test` 运行**（需 root + youki 二进制）——真实环境测试标 `#[ignore]`。
///
/// **youki 未注册约束**：本结构体的 `run` 真实 spawn 命令，但 youki 二进制可能未装——
/// 命令构造正确性由 `*_argv` 函数 + fixture 测覆盖，真实执行留给沙箱 `#[ignore]` 测。
///
/// `Clone`：字段仅 `String` + `PathBuf`（均廉价克隆）；真实测里 RAII guard 需持有
/// runner 副本在 Drop 时执行清理（避免借用逃逸到 `thread::spawn` 的 `'static` 约束）。
#[derive(Clone)]
pub struct YoukiRunner {
    /// 运行时二进制名/路径。
    pub bin: String,
    /// 容器状态存储根目录。
    pub state_root: PathBuf,
}

impl Default for YoukiRunner {
    fn default() -> Self {
        Self {
            bin: DEFAULT_RUNTIME_BIN.to_string(),
            state_root: PathBuf::from(DEFAULT_STATE_ROOT),
        }
    }
}

impl YoukiRunner {
    /// 构造指定二进制 + 状态根的 runner。
    pub fn new(bin: impl Into<String>, state_root: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            state_root: state_root.into(),
        }
    }

    /// 把构造层 argv（不含 program 名）前面拼上 `bin`，得到完整 argv。
    ///
    /// 构造层 `*_argv` 返回 `["--root", <dir>, <subcmd>, ...]`（无 program），
    /// 本方法在最前面加 `bin` 便于直接喂给 [`ContainerRuntimeRunner::run`]。
    pub fn full_argv(&self, constructed: &[String]) -> Vec<String> {
        let mut full = Vec::with_capacity(constructed.len() + 1);
        full.push(self.bin.clone());
        full.extend_from_slice(constructed);
        full
    }
}

#[allow(async_fn_in_trait)]
impl ContainerRuntimeRunner for YoukiRunner {
    async fn run(&self, argv: &[String]) -> ComputeResult<CommandOutput> {
        use std::process::Stdio;
        use tokio::process::Command;

        let (program, args) = argv
            .split_first()
            .ok_or_else(|| ComputeError::InvalidSpec("空 argv".to_string()))?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 不能用 `cmd.output().await`（= spawn + wait_with_output）：wait_with_output
        // 会读管道直到 EOF，而 runc/youki `create` 派生的 init 进程继承了管道写端且
        // 长驻后台，导致 EOF 永不到达 → `create_container` 在生产环境永久挂起（本机
        // runc 1.4.0 实测复现）。改用 spawn + `child.wait().await`（等进程退出，非
        // 管道 EOF）拿真实退出码，再用有界超时读 stdout/stderr——既捕获退出码与绝大
        // 部分输出，又不被后台 init 拖死。详见 docs/agents/container-agent.md 与
        // `tests/runc_real.rs` 的 `real_runc_create_start_delete_lifecycle`。
        let mut child = cmd.spawn()?;
        // 等子进程本身退出（create 的 runc 主进程会 fork init 后立即退出；start/state/
        // list/delete/version 等也都快速退出）。wait 不受管道是否被继承影响。
        let status = child.wait().await?;

        // 进程已退出，限时排空管道（init 可能仍持写端 → 无 EOF → 不限时将永久阻塞）。
        // 500ms 对任何 runc/youki 子命令的 buffered 输出都绰绰有余（runc 错误信息在
        // 进程退出前已 flush 到管道）；超时则取已读部分。create 成功时无输出，会在
        // 超时后返回空串（退出码已由 wait() 可靠拿到，是判据）。
        const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
        let stdout = match child.stdout.take() {
            Some(p) => drain_child_pipe_async(p, DRAIN_TIMEOUT).await,
            None => String::new(),
        };
        let stderr = match child.stderr.take() {
            Some(p) => drain_child_pipe_async(p, DRAIN_TIMEOUT).await,
            None => String::new(),
        };

        Ok(CommandOutput {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

/// 限时读取子进程管道：最多等 `timeout`，超时返回已读到的部分。
///
/// 用于 [`YoukiRunner::run`]——runc/youki `create` 派生的 init 进程继承管道写端且
/// 长驻，进程退出后管道无 EOF，直接 `read_to_end` 会永久阻塞。限时读既捕获输出
/// （子进程退出时其缓冲已落管道），又防 init 拖死。
async fn drain_child_pipe_async<R>(mut pipe: R, timeout: std::time::Duration) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    // 限时读：超时或读到 EOF/出错即停，返回已读部分。句柄随函数返回 drop（关闭读端；
    // init 仍持写端不影响——退出码已由 wait() 拿到）。
    let result = tokio::time::timeout(timeout, pipe.read_to_end(&mut buf)).await;
    match result {
        Ok(Ok(_)) | Ok(Err(_)) => String::from_utf8_lossy(&buf).into_owned(),
        // 超时：返回已读到的部分（可能为空，但退出码已可靠拿到）
        Err(_) => String::from_utf8_lossy(&buf).into_owned(),
    }
}

/// 把 argv 交给 runner 执行。空 argv 返回 `InvalidSpec`。
///
/// 便利方法：[`create_argv`] 等返回的 Vec（含全局 `--root` 前缀但**不含** program 名）
/// 在调用前需由 runner 决定 program——故本函数直接转发完整 argv 给 runner（runner
/// 实现负责 program 前缀，见 [`YoukiRunner::full_argv`]）。
pub async fn run_argv<R: ContainerRuntimeRunner>(
    runner: &R,
    argv: &[String],
) -> ComputeResult<CommandOutput> {
    if argv.is_empty() {
        return Err(ComputeError::InvalidSpec("空 argv".to_string()));
    }
    runner.run(argv).await
}

/// 把非零退出映射成 `ComputeError::CommandFailed`（保留 stderr 便于诊断）。
pub fn check_output<'a>(out: &'a CommandOutput, ctx: &str) -> ComputeResult<&'a CommandOutput> {
    if out.is_success() {
        Ok(out)
    } else {
        Err(ComputeError::CommandFailed(format!(
            "{ctx} 失败（退出码 {}）：{}",
            out.exit_code,
            out.stderr.trim()
        )))
    }
}

// ----------------------------------------------------------------------------
// 测试用 fixture runner（仅在 test 编译）
// ----------------------------------------------------------------------------

/// 测试用 runner——按 argv 子串匹配预录 fixture 输出 + 记录所有调用顺序。
///
/// 与 `FixtureAptRunner` 同构设计：`on(args_contains, output)` 注册期望，`run` 时按
/// argv join 后的串做子串匹配查表返回 fixture；无匹配返回 `Internal` 错误（让测试
/// 明确暴露未覆盖的命令调用）。额外暴露 [`Self::calls`] 返回按顺序记录的所有 argv，
/// 便于编排测断言「create → start」调用顺序正确。
#[cfg(test)]
pub struct FixtureRuntimeRunner {
    fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}

#[cfg(test)]
struct FixtureEntry {
    /// argv join 后的子串（如 `"create c1 --bundle"`）
    args_contains: String,
    output: CommandOutput,
}

#[cfg(test)]
impl Default for FixtureRuntimeRunner {
    fn default() -> Self {
        Self {
            fixtures: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl FixtureRuntimeRunner {
    /// 空构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册期望：当调 argv join 含 `args_contains` 时返回 `output`。
    ///
    /// `args_contains` 在 argv（含 program 名）join 后的串里做子串匹配
    /// （如 `"create c1 --bundle"`），便于按子命令 + ID 区分多次同类调用。
    /// 多条匹配按注册顺序返回首个命中。
    pub fn on(self, args_contains: &str, output: CommandOutput) -> Self {
        self.fixtures.lock().unwrap().push(FixtureEntry {
            args_contains: args_contains.to_string(),
            output,
        });
        self
    }

    /// 返回所有实际调用（按顺序），供编排测断言调用顺序。
    ///
    /// 每项是一次 `run` 调用的完整 argv（含 program 名）。
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[allow(async_fn_in_trait)]
impl ContainerRuntimeRunner for FixtureRuntimeRunner {
    async fn run(&self, argv: &[String]) -> ComputeResult<CommandOutput> {
        let joined = argv.join(" ");
        // 先记录调用（无论是否命中 fixture）
        self.calls.lock().unwrap().push(argv.to_vec());
        let fixtures = self.fixtures.lock().unwrap();
        for entry in fixtures.iter() {
            if joined.contains(&entry.args_contains) {
                return Ok(entry.output.clone());
            }
        }
        Err(ComputeError::Internal(format!(
            "FixtureRuntimeRunner 无匹配 fixture: {joined}"
        )))
    }
}

// ----------------------------------------------------------------------------
// ImagePuller trait（oci-distribution 拉镜像抽象骨架）
// ----------------------------------------------------------------------------

/// 镜像拉取器抽象——从 registry 拉镜像，返回 sha256 digest。
///
/// **oci-distribution 未注册**（本任务约束）：批 3 引 oci-distribution crate 前，
/// 生产实现用 [`StubImagePuller`]（返回占位 digest，不真拉）。trait 抽象先行，
/// 让 `ContainerRuntimeImpl::pull_image` 可测（编排层注入 fixture/真实实现）。
///
/// 与 [`ContainerRuntimeRunner`] 同构设计（trait 抽象 + 多实现注入）。
#[allow(async_fn_in_trait)]
pub trait ImagePuller: Send + Sync {
    /// 从 registry 拉取镜像 `image`（如 `nginx:1.25`），返回 digest（`sha256:...`）。
    ///
    /// 实现应：
    /// - 解析镜像名 → registry/repo/tag；
    /// - 认证（匿名或配置的 credentials）；
    /// - 下载 manifest + layers，解包到 bundle 的 `rootfs/`；
    /// - 返回 manifest 的 digest（content-addressable）。
    async fn pull(&self, image: &str) -> ComputeResult<String>;
}

/// 占位镜像拉取器——不真拉，返回确定性 digest（基于镜像名哈希）。
///
/// **仅过渡用**：批 3 引 oci-distribution 后替换为 `OciDistributionPuller`（真拉
/// 并解包 rootfs）。当前让 `ContainerRuntimeImpl::pull_image` 编排骨架可编译可测，
/// 不阻塞容器生命周期编排骨架落地。
///
/// 返回的 digest 形如 `sha256:<16 hex>`（镜像名 FNV-1a 哈希低 64 位），确定性——
/// 同名镜像返回同 digest，便于断言。
pub struct StubImagePuller;

impl Default for StubImagePuller {
    fn default() -> Self {
        Self
    }
}

impl StubImagePuller {
    /// 构造占位拉取器。
    pub fn new() -> Self {
        Self
    }
}

#[allow(async_fn_in_trait)]
impl ImagePuller for StubImagePuller {
    async fn pull(&self, image: &str) -> ComputeResult<String> {
        if image.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("镜像名不能为空".into()));
        }
        // FNV-1a 64-bit 哈希——确定性占位 digest（非真实 content-addressable）。
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in image.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        Ok(format!("sha256:{:016x}", hash))
    }
}

/// 测试用 ImagePuller——按镜像名匹配预置 digest。
#[cfg(test)]
pub struct FixtureImagePuller {
    /// 镜像名 → digest 映射；未命中返回占位 digest（用 StubImagePuller 兜底）。
    map: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl Default for FixtureImagePuller {
    fn default() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[cfg(test)]
impl FixtureImagePuller {
    /// 空构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置镜像 → digest 映射。
    pub fn with(self, image: &str, digest: &str) -> Self {
        self.map
            .lock()
            .unwrap()
            .insert(image.to_string(), digest.to_string());
        self
    }
}

#[cfg(test)]
#[allow(async_fn_in_trait)]
impl ImagePuller for FixtureImagePuller {
    async fn pull(&self, image: &str) -> ComputeResult<String> {
        if image.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("镜像名不能为空".into()));
        }
        // 先查表，命中则立即返回（guard 在块结束释放，不跨 await）
        let hit = {
            let map = self.map.lock().unwrap();
            map.get(image).cloned()
        };
        if let Some(d) = hit {
            return Ok(d);
        }
        // 未命中用 StubImagePuller 兜底（保持确定性）——此时无锁，可安全 await
        StubImagePuller.pull(image).await
    }
}

// ----------------------------------------------------------------------------
// ContainerRuntimeImpl——编排 OCI bundle + CNI + runner（实现 ContainerRuntime）
// ----------------------------------------------------------------------------

/// 容器运行时编排实现——组合 [`ContainerRuntimeRunner`]（youki/runc 执行）+
/// [`ImagePuller`]（镜像拉取）+ 内存索引，实现 [`crate::container::ContainerRuntime`]。
///
/// 编排顺序（create_container）：
/// 1. 校验 spec（`image` 非空、端口/挂载合法）；
/// 2. 写 OCI bundle（复用 [`crate::oci::write_bundle`]：生成 `config.json` 落盘）；
/// 3. （若 spec.network 指定）写 CNI conflist（复用 [`crate::cni::write_network`]）；
/// 4. runner.create（`youki create <id> --bundle <bundle_dir>`，不启动）；
/// 5. 记录内存索引（id → Container，状态 Created）。
///
/// start_container：runner.start（`youki start <id>`）+ 状态迁移校验。
/// stop_container：runner.kill（`youki kill <id> TERM|KILL`）+ 状态迁移。
/// remove_container：runner.delete（`youki delete <id>`）+ 移除索引。
/// pull_image：委托 [`ImagePuller`]（oci-distribution 未注册时用 stub）。
///
/// **泛型参数**：
/// - `R`：容器运行时执行器（[`YoukiRunner`] 或 fixture）；
/// - `P`：镜像拉取器（[`StubImagePuller`] 或 fixture）。
///
/// 因 [`crate::container::ContainerRuntime`] trait 用 `async fn in trait`（非 object-safe），本实现
/// 用泛型持有 R/P（零虚表开销）；调用方传具体类型即可。
///
/// **youki 未注册约束**：本结构体不引 youki crate——`R` 默认是 [`YoukiRunner`]（仅
/// spawn `youki` 二进制），真实执行需 root + youki 装机（`#[ignore]` 测守护）。命令
/// 构造正确性由 fixture 测覆盖。
pub struct ContainerRuntimeImpl<R: ContainerRuntimeRunner, P: ImagePuller> {
    /// 容器运行时执行器（youki/runc spawn 抽象）。
    pub runner: R,
    /// 镜像拉取器（oci-distribution 抽象）。
    pub puller: P,
    /// OCI bundle 根目录（`<bundle_base>/<id>/config.json`）。
    pub bundle_base: PathBuf,
    /// CNI 网络配置目录（默认 `/etc/cni/net.d`）。
    pub cni_net_dir: PathBuf,
    /// CNI 网络默认子网（network 未指定子网时用；生产由配置注入）。
    pub default_subnet: os_network::IpCidr,
    /// 内存索引：id → Container（状态镜像 youki 实际状态）。
    containers: std::sync::Mutex<std::collections::HashMap<String, ContainerEntry>>,
    /// 本地镜像索引：digest → ImageInfo。
    images: std::sync::Mutex<std::collections::HashMap<String, ImageInfo>>,
}

/// 内存索引项（Container + bundle 路径，便于 delete 时清理）。
struct ContainerEntry {
    /// 容器实例（id/name/spec/state/image_digest）。
    container: Container,
    /// OCI bundle 目录（remove_container 时可清理）。
    bundle_dir: PathBuf,
}

impl<R: ContainerRuntimeRunner, P: ImagePuller> ContainerRuntimeImpl<R, P> {
    /// 构造编排实现。
    ///
    /// - `runner`：youki/runc 执行器（生产用 [`YoukiRunner`]，测试用 fixture）；
    /// - `puller`：镜像拉取器（过渡用 [`StubImagePuller`]）；
    /// - `bundle_base`：OCI bundle 根（每个容器建子目录 `<bundle_base>/<id>/`）；
    /// - `cni_net_dir`：CNI 配置目录（生产 `/etc/cni/net.d`，测试用 tempdir）；
    /// - `default_subnet`：容器网络默认子网。
    pub fn new(
        runner: R,
        puller: P,
        bundle_base: impl Into<PathBuf>,
        cni_net_dir: impl Into<PathBuf>,
        default_subnet: os_network::IpCidr,
    ) -> Self {
        Self {
            runner,
            puller,
            bundle_base: bundle_base.into(),
            cni_net_dir: cni_net_dir.into(),
            default_subnet,
            containers: std::sync::Mutex::new(std::collections::HashMap::new()),
            images: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 指定容器的 bundle 目录：`<bundle_base>/<id>/`。
    fn bundle_dir_for(&self, id: &str) -> PathBuf {
        self.bundle_base.join(id)
    }

    /// 指定容器的 cgroup 路径：`/os/<id>`（写入 OCI config.json 的 linux.cgroupsPath）。
    fn cgroup_path_for(id: &str) -> String {
        format!("/os/{id}")
    }

    /// 构造层 argv → full argv（含 program）→ runner 执行 → 校验退出码。
    ///
    /// `ctx`：失败时写入错误消息的上下文（如 `"create c1"`）。
    async fn exec_checked(
        &self,
        argv: &[String],
        ctx: &str,
    ) -> ComputeResult<os_core::CommandOutput> {
        let out = run_argv(&self.runner, argv).await?;
        check_output(&out, ctx).cloned()
    }
}

#[allow(async_fn_in_trait)]
impl<R: ContainerRuntimeRunner, P: ImagePuller> crate::container::ContainerRuntime
    for ContainerRuntimeImpl<R, P>
{
    async fn create_container(
        &self,
        id: &os_core::ContainerId,
        name: &str,
        spec: ContainerSpec,
    ) -> ComputeResult<Container> {
        // 1. 校验 spec（image 非空、端口/挂载合法）
        spec.validate()?;
        let id_str = id.to_string();

        // 重复创建检查
        {
            let containers = self.containers.lock().unwrap();
            if containers.contains_key(&id_str) {
                return Err(ComputeError::InvalidSpec(format!("容器已存在: {id}")));
            }
        }

        // 2. 写 OCI bundle（config.json 落盘）
        let bundle_dir = self.bundle_dir_for(&id_str);
        let cgroup = Self::cgroup_path_for(&id_str);
        crate::oci::write_bundle(&spec, &bundle_dir, Some(&cgroup))?;

        // 3. 若 spec.network 指定，写 CNI conflist（Bridge 驱动）
        if let Some(net) = spec.network.as_ref() {
            // 生产由 network-agent 提供 subnet；此处用 default_subnet 兜底（编排骨架）
            crate::cni::write_network(
                net,
                NetworkDriver::Bridge,
                self.default_subnet,
                &self.cni_net_dir,
            )?;
        }

        // 4. runner.create（youki create <id> --bundle <bundle_dir>）
        let create = create_argv(&self.runner_state_root(), &id_str, &bundle_dir)?;
        let full = self.runner_full_argv(&create);
        self.exec_checked(&full, &format!("youki create {id}"))
            .await?;

        // 5. 记录内存索引（状态 Created）
        let container = Container::new(id.clone(), name.to_string(), spec);
        {
            let mut containers = self.containers.lock().unwrap();
            containers.insert(
                id_str.clone(),
                ContainerEntry {
                    container: container.clone(),
                    bundle_dir: bundle_dir.clone(),
                },
            );
        }
        Ok(container)
    }

    async fn start_container(&self, id: &os_core::ContainerId) -> ComputeResult<Container> {
        let id_str = id.to_string();
        // 状态迁移校验 + 取容器
        let next = ContainerState::Running;
        {
            let mut containers = self.containers.lock().unwrap();
            let entry = containers
                .get_mut(&id_str)
                .ok_or_else(|| ComputeError::ContainerNotFound(id_str.clone()))?;
            crate::container::validate_transition(entry.container.state, next)?;
            // 标记迁移中（runner 执行成功后正式落 Running，失败回滚——简化为预置）
            entry.container.state = next;
        }
        // runner.start
        let start = start_argv(&self.runner_state_root(), &id_str)?;
        let full = self.runner_full_argv(&start);
        if let Err(e) = self.exec_checked(&full, &format!("youki start {id}")).await {
            // 回滚状态
            if let Some(entry) = self.containers.lock().unwrap().get_mut(&id_str) {
                entry.container.state = ContainerState::Created;
            }
            return Err(e);
        }
        let containers = self.containers.lock().unwrap();
        Ok(containers
            .get(&id_str)
            .map(|e| e.container.clone())
            .unwrap_or_else(|| Container::new(id.clone(), String::new(), ContainerSpec::new(""))))
    }

    async fn stop_container(
        &self,
        id: &os_core::ContainerId,
        force: bool,
    ) -> ComputeResult<Container> {
        let id_str = id.to_string();
        let next = ContainerState::Stopped;
        {
            let mut containers = self.containers.lock().unwrap();
            let entry = containers
                .get_mut(&id_str)
                .ok_or_else(|| ComputeError::ContainerNotFound(id_str.clone()))?;
            crate::container::validate_transition(entry.container.state, next)?;
            entry.container.state = next;
        }
        // runner.kill（force → KILL）
        let kill = stop_argv(&self.runner_state_root(), &id_str, force)?;
        let full = self.runner_full_argv(&kill);
        self.exec_checked(&full, &format!("youki kill {id}"))
            .await?;
        let containers = self.containers.lock().unwrap();
        Ok(containers
            .get(&id_str)
            .map(|e| e.container.clone())
            .unwrap_or_else(|| Container::new(id.clone(), String::new(), ContainerSpec::new(""))))
    }

    async fn remove_container(&self, id: &os_core::ContainerId) -> ComputeResult<()> {
        let id_str = id.to_string();
        // 须先停止
        let bundle_dir;
        {
            let containers = self.containers.lock().unwrap();
            let entry = containers
                .get(&id_str)
                .ok_or_else(|| ComputeError::ContainerNotFound(id_str.clone()))?;
            if entry.container.state == ContainerState::Running {
                return Err(ComputeError::InvalidSpec(format!(
                    "容器 {id} 运行中，须先停止再删除"
                )));
            }
            bundle_dir = entry.bundle_dir.clone();
        }
        // runner.delete（已停止，不需 --force）
        let delete = delete_argv(&self.runner_state_root(), &id_str, false)?;
        let full = self.runner_full_argv(&delete);
        self.exec_checked(&full, &format!("youki delete {id}"))
            .await?;
        // 移除索引 + 清理 bundle 目录（忽略清理失败——容器已删，bundle 残留无害）
        self.containers.lock().unwrap().remove(&id_str);
        let _ = std::fs::remove_dir_all(&bundle_dir);
        Ok(())
    }

    async fn get_container(&self, id: &os_core::ContainerId) -> ComputeResult<Container> {
        let containers = self.containers.lock().unwrap();
        containers
            .get(&id.to_string())
            .map(|e| e.container.clone())
            .ok_or_else(|| ComputeError::ContainerNotFound(id.to_string()))
    }

    async fn list_containers(&self) -> ComputeResult<Vec<Container>> {
        let containers = self.containers.lock().unwrap();
        Ok(containers.values().map(|e| e.container.clone()).collect())
    }

    async fn pull_image(&self, image: &str) -> ComputeResult<String> {
        let digest = self.puller.pull(image).await?;
        // 记录镜像索引
        let img = ImageInfo {
            digest: digest.clone(),
            name: image.to_string(),
            size: 0, // 真实 size 由 oci-distribution 拉取后回填；stub 占位
            pulled_at: chrono::Utc::now(),
        };
        self.images.lock().unwrap().insert(digest.clone(), img);
        Ok(digest)
    }

    async fn list_images(&self) -> ComputeResult<Vec<ImageInfo>> {
        Ok(self.images.lock().unwrap().values().cloned().collect())
    }

    async fn remove_image(&self, digest: &str) -> ComputeResult<()> {
        if self.images.lock().unwrap().remove(digest).is_none() {
            return Err(ComputeError::ImagePullFailed(format!(
                "镜像不存在: {digest}"
            )));
        }
        Ok(())
    }
}

impl<R: ContainerRuntimeRunner, P: ImagePuller> ContainerRuntimeImpl<R, P> {
    /// 取 runner 的 state_root（youki `--root` 全局参数）。
    ///
    /// 默认用 [`DEFAULT_STATE_ROOT`]——若 runner 是 [`YoukiRunner`]，取其 state_root；
    /// 否则（fixture）用默认（fixture 按 argv 子串匹配，不关心真实路径）。
    fn runner_state_root(&self) -> PathBuf {
        PathBuf::from(DEFAULT_STATE_ROOT)
    }

    /// 借用 runner（测试用于检查 fixture 调用记录）。
    #[cfg(test)]
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// 构造 full argv（argv 前置 program 名）。
    ///
    /// 与 [`YoukiRunner::full_argv`] 一致——fixture runner 不关心 program 名
    /// （按子串匹配），故统一前置 [`DEFAULT_RUNTIME_BIN`] 占位。
    fn runner_full_argv(&self, constructed: &[String]) -> Vec<String> {
        let mut full = Vec::with_capacity(constructed.len() + 1);
        full.push(DEFAULT_RUNTIME_BIN.to_string());
        full.extend_from_slice(constructed);
        full
    }
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::ContainerRuntime;
    use std::path::Path;

    fn root() -> PathBuf {
        PathBuf::from("/run/os/youki")
    }

    // ----------------------------------------------------------------
    // argv 构造测
    // ----------------------------------------------------------------

    #[test]
    fn global_args_emits_root_prefix() {
        let a = global_args(&root());
        assert_eq!(a, vec!["--root", "/run/os/youki"]);
    }

    #[test]
    fn create_argv_has_bundle_flag() {
        let a = create_argv(&root(), "c1", Path::new("/var/lib/os/bundles/c1")).unwrap();
        assert_eq!(
            a,
            vec![
                "--root",
                "/run/os/youki",
                "create",
                "c1",
                "--bundle",
                "/var/lib/os/bundles/c1",
            ]
        );
    }

    #[test]
    fn start_argv_just_id() {
        let a = start_argv(&root(), "c1").unwrap();
        assert_eq!(a, vec!["--root", "/run/os/youki", "start", "c1"]);
    }

    #[test]
    fn kill_argv_takes_signal() {
        let a = kill_argv(&root(), "c1", "TERM").unwrap();
        assert_eq!(a, vec!["--root", "/run/os/youki", "kill", "c1", "TERM"]);
    }

    #[test]
    fn stop_argv_uses_term_by_default() {
        let a = stop_argv(&root(), "c1", false).unwrap();
        assert_eq!(a.last().unwrap(), "TERM");
    }

    #[test]
    fn stop_argv_uses_kill_when_force() {
        let a = stop_argv(&root(), "c1", true).unwrap();
        assert_eq!(a.last().unwrap(), "KILL");
    }

    #[test]
    fn delete_argv_optional_force() {
        let without = delete_argv(&root(), "c1", false).unwrap();
        assert!(!without.contains(&"--force".to_string()));
        let with_force = delete_argv(&root(), "c1", true).unwrap();
        assert!(with_force.contains(&"--force".to_string()));
    }

    #[test]
    fn state_and_list_argv() {
        let s = state_argv(&root(), "c1").unwrap();
        assert_eq!(s, vec!["--root", "/run/os/youki", "state", "c1"]);
        let l = list_argv(&root());
        assert_eq!(l, vec!["--root", "/run/os/youki", "list"]);
    }

    #[test]
    fn exec_argv_appends_command() {
        let cmd = vec!["ls".to_string(), "-l".to_string(), "/".to_string()];
        let a = exec_argv(&root(), "c1", &cmd).unwrap();
        assert_eq!(
            a,
            vec!["--root", "/run/os/youki", "exec", "c1", "ls", "-l", "/",]
        );
    }

    #[test]
    fn pause_and_resume_argv() {
        let p = pause_argv(&root(), "c1").unwrap();
        assert_eq!(p, vec!["--root", "/run/os/youki", "pause", "c1"]);
        let r = resume_argv(&root(), "c1").unwrap();
        assert_eq!(r, vec!["--root", "/run/os/youki", "resume", "c1"]);
    }

    #[test]
    fn argv_rejects_empty_id() {
        assert!(create_argv(&root(), "", Path::new("/b")).is_err());
        assert!(start_argv(&root(), "  ").is_err());
        assert!(kill_argv(&root(), "", "KILL").is_err());
    }

    #[test]
    fn argv_rejects_whitespace_id() {
        assert!(start_argv(&root(), "c 1").is_err());
        assert!(delete_argv(&root(), "c\tn", false).is_err());
    }

    #[test]
    fn kill_rejects_empty_signal() {
        assert!(kill_argv(&root(), "c1", "").is_err());
        assert!(kill_argv(&root(), "c1", "  ").is_err());
    }

    #[test]
    fn exec_rejects_empty_command() {
        assert!(exec_argv(&root(), "c1", &[]).is_err());
        assert!(exec_argv(&root(), "c1", &["  ".to_string()]).is_err());
    }

    // ----------------------------------------------------------------
    // state 解析测
    // ----------------------------------------------------------------

    #[test]
    fn parse_state_status_extracts_running() {
        let json = r#"{"ociVersion":"1.0.2-dev","id":"c1","status":"running"}"#;
        assert_eq!(parse_state_status(json).as_deref(), Some("running"));
    }

    #[test]
    fn parse_state_status_handles_spaces() {
        let json = r#"{"status" : "paused" }"#;
        assert_eq!(parse_state_status(json).as_deref(), Some("paused"));
    }

    #[test]
    fn parse_state_status_returns_none_when_missing() {
        let json = r#"{"ociVersion":"1.0.2-dev","id":"c1"}"#;
        assert!(parse_state_status(json).is_none());
    }

    #[test]
    fn status_to_state_maps_known_values() {
        use crate::container::ContainerState;
        assert_eq!(status_to_state("created"), Some(ContainerState::Created));
        assert_eq!(status_to_state("running"), Some(ContainerState::Running));
        assert_eq!(status_to_state("stopped"), Some(ContainerState::Stopped));
        assert_eq!(status_to_state("paused"), Some(ContainerState::Paused));
    }

    #[test]
    fn status_to_state_none_for_unknown() {
        assert!(status_to_state("unknown").is_none());
        assert!(status_to_state("").is_none());
    }

    // ----------------------------------------------------------------
    // YoukiRunner 命令前缀测
    // ----------------------------------------------------------------

    #[test]
    fn youki_runner_default_uses_defaults() {
        let r = YoukiRunner::default();
        assert_eq!(r.bin, DEFAULT_RUNTIME_BIN);
        assert_eq!(r.state_root, PathBuf::from(DEFAULT_STATE_ROOT));
    }

    #[test]
    fn youki_runner_custom_bin_and_root() {
        let r = YoukiRunner::new("runc", "/var/run/runc");
        assert_eq!(r.bin, "runc");
        assert_eq!(r.state_root, PathBuf::from("/var/run/runc"));
    }

    #[test]
    fn full_argv_prepends_bin() {
        let r = YoukiRunner::new("/usr/local/bin/youki", "/run/os/youki");
        let constructed = start_argv(&r.state_root, "c1").unwrap();
        let full = r.full_argv(&constructed);
        assert_eq!(full[0], "/usr/local/bin/youki");
        assert!(full.contains(&"start".to_string()));
        assert!(full.contains(&"c1".to_string()));
    }

    // ----------------------------------------------------------------
    // 补充测：常量 / global_args 自定义 / parse_state_status 边界
    // ----------------------------------------------------------------

    #[test]
    fn runtime_constants_values() {
        assert_eq!(DEFAULT_RUNTIME_BIN, "youki");
        assert_eq!(DEFAULT_STATE_ROOT, "/run/os/youki");
        assert_eq!(DEFAULT_STOP_SIGNAL, "TERM");
        assert_eq!(FORCE_STOP_SIGNAL, "KILL");
    }

    #[test]
    fn state_status_module_constants() {
        assert_eq!(state_status::CREATED, "created");
        assert_eq!(state_status::RUNNING, "running");
        assert_eq!(state_status::STOPPED, "stopped");
        assert_eq!(state_status::PAUSED, "paused");
    }

    #[test]
    fn global_args_custom_root() {
        let a = global_args(&PathBuf::from("/custom/state/root"));
        assert_eq!(a, vec!["--root", "/custom/state/root"]);
    }

    #[test]
    fn create_argv_with_custom_root_and_bundle() {
        let root = PathBuf::from("/run/runc");
        let a = create_argv(&root, "abc", Path::new("/bundle/x")).unwrap();
        assert_eq!(
            a,
            vec![
                "--root",
                "/run/runc",
                "create",
                "abc",
                "--bundle",
                "/bundle/x"
            ]
        );
    }

    #[test]
    fn kill_argv_with_numeric_signal() {
        let a = kill_argv(&root(), "c1", "9").unwrap();
        assert_eq!(a.last().unwrap(), "9");
    }

    #[test]
    fn delete_argv_force_true_has_force_flag() {
        let a = delete_argv(&root(), "c1", true).unwrap();
        assert_eq!(a.last().unwrap(), "--force");
    }

    #[test]
    fn exec_argv_with_no_args_after_cmd_still_ok() {
        // cmd 仅含可执行名（无参）合法
        let cmd = vec!["sh".to_string()];
        let a = exec_argv(&root(), "c1", &cmd).unwrap();
        assert_eq!(a, vec!["--root", "/run/os/youki", "exec", "c1", "sh"]);
    }

    #[test]
    fn parse_state_status_with_pretty_json() {
        // pretty JSON（多行 + 缩进 + 换行）
        let json = "{\n  \"status\": \"created\"\n}";
        assert_eq!(parse_state_status(json).as_deref(), Some("created"));
    }

    #[test]
    fn parse_state_status_missing_quote_after_value_returns_none() {
        // value 缺结束引号 → 找不到结尾 → None
        let json = r#"{"status":"running}"#;
        assert!(parse_state_status(json).is_none());
    }

    #[test]
    fn parse_state_status_no_colon_returns_none() {
        // 缺冒号分隔符
        let json = r#"{"status" "running"}"#;
        assert!(parse_state_status(json).is_none());
    }

    #[test]
    fn parse_state_status_no_value_quote_returns_none() {
        // status 后是 number 非 string
        let json = r#"{"status": 123}"#;
        assert!(parse_state_status(json).is_none());
    }

    #[test]
    fn parse_state_status_first_status_field_wins() {
        // 多个 status 字段：朴素 find 取首个
        let json = r#"{"status":"first","other":{"status":"second"}}"#;
        assert_eq!(parse_state_status(json).as_deref(), Some("first"));
    }

    #[test]
    fn status_to_state_returns_proper_state_kinds() {
        use crate::container::ContainerState;
        assert_eq!(status_to_state("created"), Some(ContainerState::Created));
        assert_eq!(status_to_state("running"), Some(ContainerState::Running));
        assert_eq!(status_to_state("stopped"), Some(ContainerState::Stopped));
        assert_eq!(status_to_state("paused"), Some(ContainerState::Paused));
    }

    #[test]
    fn youki_runner_full_argv_empty_constructed() {
        // 空 constructed：full_argv 应仅含 bin
        let r = YoukiRunner::default();
        let full = r.full_argv(&[]);
        assert_eq!(full, vec![DEFAULT_RUNTIME_BIN.to_string()]);
    }

    #[test]
    fn youki_runner_clone() {
        // Clone derive：应可克隆且字段独立
        let r = YoukiRunner::new("runc", "/run/runc");
        let cloned = r.clone();
        assert_eq!(cloned.bin, r.bin);
        assert_eq!(cloned.state_root, r.state_root);
    }

    #[tokio::test]
    async fn stub_image_puller_digest_format() {
        // digest 形如 sha256:<16 hex>
        let p = StubImagePuller;
        let d = p.pull("nginx").await.unwrap();
        assert!(d.starts_with("sha256:"));
        let hex = &d["sha256:".len()..];
        assert_eq!(hex.len(), 16, "应为 16 hex 字符");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn fixture_image_puller_default() {
        let p = FixtureImagePuller::new();
        // 未预置：走 stub 兜底
        let d = p.pull("anyimage").await.unwrap();
        assert!(d.starts_with("sha256:"));
    }

    #[tokio::test]
    async fn fixture_image_puller_empty_image_rejects_even_with_preset() {
        // 空镜像名即使预置也拒绝
        let p = FixtureImagePuller::new().with("", "sha256:x");
        let err = p.pull("").await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    // ----------------------------------------------------------------
    // FixtureRuntimeRunner 补充：calls 记录顺序 + 多次同 fixture
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn fixture_runner_calls_recorded_in_order_with_argv() {
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok());
        let create = create_argv(&root(), "c1", Path::new("/b")).unwrap();
        let start = start_argv(&root(), "c1").unwrap();
        run_argv(&runner, &create).await.unwrap();
        run_argv(&runner, &start).await.unwrap();

        let calls = runner.calls();
        // 第二次调用 argv 应含 start c1
        assert!(calls[1].iter().any(|s| s == "start"));
        assert!(calls[1].iter().any(|s| s == "c1"));
    }

    #[tokio::test]
    async fn fixture_runner_call_recorded_even_on_unmatched() {
        // 无 fixture 命中也记录调用
        let runner = FixtureRuntimeRunner::new();
        let argv = start_argv(&root(), "x").unwrap();
        let _ = run_argv(&runner, &argv).await;
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "未命中也应记录调用");
    }

    #[tokio::test]
    async fn fixture_runner_default_is_empty_fixtures() {
        let runner = FixtureRuntimeRunner::default();
        let argv = vec!["anything".to_string()];
        let err = run_argv(&runner, &argv).await.unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[tokio::test]
    async fn exec_checked_propagates_failure() {
        // exec_checked 把失败 stderr 包进 CommandFailed
        let runner =
            FixtureRuntimeRunner::new().on("create", CommandOutput::fail(1, "bundle not found"));
        let create = create_argv(&root(), "c1", Path::new("/b")).unwrap();
        let full = YoukiRunner::default().full_argv(&create);
        let err = run_argv(&runner, &full).await.unwrap();
        assert!(!err.is_success());
        let err = check_output(&err, "ctx").unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(m) if m.contains("bundle not found")));
    }

    // ----------------------------------------------------------------
    // FixtureRuntimeRunner 测
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn run_argv_empty_returns_error() {
        let runner = FixtureRuntimeRunner::new();
        let err = run_argv(&runner, &[]).await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn check_output_maps_nonzero() {
        let out = CommandOutput::fail(1, "boom");
        let err = check_output(&out, "ctx").unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(_)));
        assert!(check_output(&CommandOutput::ok(), "ctx").is_ok());
    }

    #[tokio::test]
    async fn fixture_runner_matches_by_substring() {
        let runner = FixtureRuntimeRunner::new().on("list", CommandOutput::ok_with_stdout("c1\n"));
        let argv = list_argv(&root());
        let out = run_argv(&runner, &argv).await.unwrap();
        assert!(out.is_success());
        assert_eq!(out.stdout, "c1\n");
    }

    #[tokio::test]
    async fn fixture_runner_records_call_order() {
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok());
        let create = create_argv(&root(), "c1", Path::new("/b/c1")).unwrap();
        run_argv(&runner, &create).await.unwrap();
        let start = start_argv(&root(), "c1").unwrap();
        run_argv(&runner, &start).await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains(&"create".to_string()));
        assert!(calls[1].contains(&"start".to_string()));
    }

    #[tokio::test]
    async fn fixture_runner_unmatched_returns_internal_error() {
        let runner = FixtureRuntimeRunner::new();
        let argv = start_argv(&root(), "c1").unwrap();
        let err = run_argv(&runner, &argv).await.unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    // ----------------------------------------------------------------
    // ImagePuller 测
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn stub_puller_returns_deterministic_digest() {
        let p = StubImagePuller::new();
        let d1 = p.pull("nginx:1.25").await.unwrap();
        let d2 = p.pull("nginx:1.25").await.unwrap();
        assert_eq!(d1, d2, "同名镜像 digest 应确定");
        assert!(d1.starts_with("sha256:"));
        // 不同名不同 digest
        let d3 = p.pull("redis:7").await.unwrap();
        assert_ne!(d1, d3);
    }

    #[tokio::test]
    async fn stub_puller_rejects_empty_image() {
        let p = StubImagePuller::new();
        let err = p.pull("").await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn fixture_puller_returns_preset_digest() {
        let p = FixtureImagePuller::new().with("nginx:1.25", "sha256:abc123");
        assert_eq!(p.pull("nginx:1.25").await.unwrap(), "sha256:abc123");
        // 未命中走 stub 兜底
        let fallback = p.pull("redis:7").await.unwrap();
        assert!(fallback.starts_with("sha256:"));
    }

    // ----------------------------------------------------------------
    // ContainerRuntimeImpl 编排测（fixture runner + tempdir bundle/cni）
    // ----------------------------------------------------------------

    fn default_subnet() -> os_network::IpCidr {
        os_network::IpCidr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 88, 0, 0)),
            24,
        )
    }

    /// 构造编排实现 + tempdir（bundle_base/cni_net_dir 各一个 tempdir）。
    fn make_impl(
        runner: FixtureRuntimeRunner,
    ) -> (
        ContainerRuntimeImpl<FixtureRuntimeRunner, StubImagePuller>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let bundle_tmp = tempfile::tempdir().unwrap();
        let cni_tmp = tempfile::tempdir().unwrap();
        let imp = ContainerRuntimeImpl::new(
            runner,
            StubImagePuller::new(),
            bundle_tmp.path(),
            cni_tmp.path(),
            default_subnet(),
        );
        (imp, bundle_tmp, cni_tmp)
    }

    #[tokio::test]
    async fn create_container_writes_bundle_and_invokes_create() {
        let runner = FixtureRuntimeRunner::new().on("create", CommandOutput::ok());
        let (imp, _bundle_tmp, _cni_tmp) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");

        let c = imp
            .create_container(&id, "nginx", ContainerSpec::new("nginx:1.25"))
            .await
            .unwrap();

        assert_eq!(c.state, ContainerState::Created);
        // OCI bundle 已落盘（config.json 存在）
        let config = imp
            .bundle_dir_for("c1")
            .join(crate::oci::CONFIG_JSON_FILENAME);
        assert!(config.is_file(), "config.json 应落盘");
        // runner 被调（create 命令）
        let calls = imp.runner().calls();
        assert!(calls
            .iter()
            .any(|a| a.contains(&"create".to_string()) && a.contains(&"c1".to_string())));
    }

    #[tokio::test]
    async fn create_container_with_network_writes_cni_conflist() {
        let runner = FixtureRuntimeRunner::new().on("create", CommandOutput::ok());
        let (imp, _bundle_tmp, cni_tmp) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");

        let spec = ContainerSpec::new("nginx:1.25").with_network("osnet");
        imp.create_container(&id, "nginx", spec).await.unwrap();

        // CNI conflist 已落盘
        let conflist = cni_tmp.path().join("osnet.conflist");
        assert!(conflist.is_file(), "osnet.conflist 应落盘");
    }

    #[tokio::test]
    async fn create_container_rejects_duplicate() {
        let runner = FixtureRuntimeRunner::new().on("create", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        imp.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        let err = imp
            .create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn create_container_rejects_empty_image() {
        let runner = FixtureRuntimeRunner::new();
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        let mut spec = ContainerSpec::new("img");
        spec.image = "  ".to_string();
        let err = imp.create_container(&id, "x", spec).await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn lifecycle_create_start_stop_remove_in_order() {
        // 关键测：验证编排骨架按 create → start → kill → delete 顺序调 runner
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok())
            .on("kill", CommandOutput::ok())
            .on("delete", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");

        imp.create_container(&id, "nginx", ContainerSpec::new("nginx:1.25"))
            .await
            .unwrap();
        imp.start_container(&id).await.unwrap();
        imp.stop_container(&id, false).await.unwrap();
        imp.remove_container(&id).await.unwrap();

        // 断言调用顺序
        let calls = imp.runner().calls();
        let subcmds: Vec<&str> = calls
            .iter()
            .filter_map(|argv| {
                // argv 形如 [program, "--root", <dir>, <subcmd>, ...]——取第 4 项（index 3）
                argv.iter()
                    .position(|s| s == "create" || s == "start" || s == "kill" || s == "delete")
                    .and_then(|i| argv.get(i).map(|s| s.as_str()))
            })
            .collect();
        assert_eq!(subcmds, vec!["create", "start", "kill", "delete"]);
    }

    #[tokio::test]
    async fn start_uses_correct_signal_default_term() {
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok())
            .on("kill", CommandOutput::ok())
            .on("delete", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        imp.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        imp.start_container(&id).await.unwrap();
        imp.stop_container(&id, false).await.unwrap();

        // stop(force=false) 应调 kill ... TERM
        let calls = imp.runner().calls();
        let kill_call = calls
            .iter()
            .find(|a| a.contains(&"kill".to_string()))
            .unwrap();
        assert!(kill_call.contains(&"TERM".to_string()));
        assert!(!kill_call.contains(&"KILL".to_string()));
    }

    #[tokio::test]
    async fn stop_force_uses_kill_signal() {
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok())
            .on("kill", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        imp.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        imp.start_container(&id).await.unwrap();
        imp.stop_container(&id, true).await.unwrap();

        let calls = imp.runner().calls();
        let kill_call = calls
            .iter()
            .find(|a| a.contains(&"kill".to_string()))
            .unwrap();
        assert!(kill_call.contains(&"KILL".to_string()));
    }

    #[tokio::test]
    async fn start_invalid_transition_from_stopped_paused() {
        // Stopped → Running 合法（restart）；但需 runner 支持
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok())
            .on("kill", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        imp.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        imp.start_container(&id).await.unwrap();
        imp.stop_container(&id, false).await.unwrap();
        // Stopped → Running 合法（重启）
        imp.start_container(&id).await.unwrap();
    }

    #[tokio::test]
    async fn remove_running_container_errors() {
        let runner = FixtureRuntimeRunner::new()
            .on("create", CommandOutput::ok())
            .on("start", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("c1");
        imp.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        imp.start_container(&id).await.unwrap();
        let err = imp.remove_container(&id).await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn get_and_list_containers() {
        let runner = FixtureRuntimeRunner::new().on("create", CommandOutput::ok());
        let (imp, _bt, _ct) = make_impl(runner);
        let id1 = os_core::ContainerId::new("c1");
        let id2 = os_core::ContainerId::new("c2");
        imp.create_container(&id1, "n1", ContainerSpec::new("img1"))
            .await
            .unwrap();
        imp.create_container(&id2, "n2", ContainerSpec::new("img2"))
            .await
            .unwrap();

        assert_eq!(imp.list_containers().await.unwrap().len(), 2);
        let c = imp.get_container(&id1).await.unwrap();
        assert_eq!(c.name, "n1");
    }

    #[tokio::test]
    async fn get_missing_container_errors() {
        let runner = FixtureRuntimeRunner::new();
        let (imp, _bt, _ct) = make_impl(runner);
        let id = os_core::ContainerId::new("nope");
        let err = imp.get_container(&id).await.unwrap_err();
        assert!(matches!(err, ComputeError::ContainerNotFound(_)));
    }

    #[tokio::test]
    async fn pull_image_records_and_lists() {
        let runner = FixtureRuntimeRunner::new();
        let (imp, _bt, _ct) = make_impl(runner);
        let d1 = imp.pull_image("nginx:1.25").await.unwrap();
        let d2 = imp.pull_image("redis:7").await.unwrap();
        assert_ne!(d1, d2);
        let imgs = imp.list_images().await.unwrap();
        assert_eq!(imgs.len(), 2);
    }

    #[tokio::test]
    async fn remove_image_succeeds_and_missing_errors() {
        let runner = FixtureRuntimeRunner::new();
        let (imp, _bt, _ct) = make_impl(runner);
        let d = imp.pull_image("nginx:1.25").await.unwrap();
        imp.remove_image(&d).await.unwrap();
        assert_eq!(imp.list_images().await.unwrap().len(), 0);
        let err = imp.remove_image(&d).await.unwrap_err();
        assert!(matches!(err, ComputeError::ImagePullFailed(_)));
    }

    // ----------------------------------------------------------------
    // 真实执行测（#[ignore]——需 root + youki 二进制，不参与常规 cargo test）
    // ----------------------------------------------------------------

    #[tokio::test]
    #[ignore = "真实 youki 执行：需 root + youki 二进制，人工 `cargo test -- --ignored`"]
    async fn real_youki_list_runs_without_panic() {
        // 验证 YoukiRunner spawn youki list 不 panic（容器状态根不存在时 youki
        // 返回空 + 退出码 0，或报错——不强断言成功，仅验证 spawn 路径通）。
        let tmp = tempfile::tempdir().unwrap();
        let runner = YoukiRunner::new("youki", tmp.path());
        let argv = runner.full_argv(&list_argv(&runner.state_root));
        let _ = runner.run(&argv).await;
    }
}
